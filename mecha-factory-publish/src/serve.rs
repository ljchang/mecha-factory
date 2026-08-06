//! A local preview server that sends the **real** headers.
//!
//! Not a convenience. A bundle checked without its Content-Security-Policy is a
//! bundle checked against something the world never sees, and that produces a
//! confident wrong answer — worse than no check. The compute class is where
//! this bites: a notebook loads its Python runtime, its workers and its
//! WebAssembly at *runtime*, so nothing static can tell you whether it boots
//! under `script-src 'self' 'wasm-unsafe-eval'` / `connect-src 'self'`. You
//! have to serve it that way and look.
//!
//! The headers come from [`mecha_manifest::ContentClass`], which is also where
//! the public server will take them from. One definition, or "verified locally"
//! means verified against a different policy.
//!
//! Deliberately small and deliberately not the public server: single-threaded,
//! loopback-only, no TLS, no range requests, no compression. It exists so a
//! human or a headless browser can look at a bundle the way a reader would.

use anyhow::{Context, Result};
use mecha_manifest::ContentClass;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};

pub struct Preview {
    listener: TcpListener,
    root: PathBuf,
    class: ContentClass,
    /// Serve without the policy, to find out what a bundle *needs*.
    ///
    /// A diagnostic, never a verification: a bundle that works with this on has
    /// been told nothing about whether it works. It exists because the first
    /// thing a real CSP does to an unprepared bundle is stop it at the first
    /// blocked script, which hides everything behind that — and knowing the
    /// full set of requirements is how you fix it in one pass instead of six.
    pub without_policy: bool,
}

impl Preview {
    /// Bind to loopback. Port 0 asks the OS for a free one, which is what the
    /// tests use so two runs cannot collide.
    pub fn bind(root: &Path, class: ContentClass, port: u16) -> Result<Self> {
        anyhow::ensure!(root.is_dir(), "{} is not a directory", root.display());
        // Loopback only, never 0.0.0.0: this serves an unpublished bundle with
        // no authentication of any kind, and binding it to every interface
        // would put a private draft on the network.
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .with_context(|| format!("binding 127.0.0.1:{port}"))?;
        Ok(Preview {
            listener,
            root: root.canonicalize()?,
            class,
            without_policy: false,
        })
    }

    pub fn addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    pub fn url(&self) -> Result<String> {
        Ok(format!("http://{}/", self.addr()?))
    }

    /// Serve until the process is stopped.
    pub fn serve_forever(&self) -> Result<()> {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    if let Err(e) = self.handle(stream) {
                        eprintln!("preview: {e:#}");
                    }
                }
                Err(e) => eprintln!("preview: accept failed: {e}"),
            }
        }
        Ok(())
    }

    /// Serve `n` requests and stop. What a test drives.
    pub fn serve(&self, n: usize) -> Result<()> {
        for _ in 0..n {
            let (stream, _) = self.listener.accept()?;
            self.handle(stream)?;
        }
        Ok(())
    }

    fn handle(&self, mut stream: TcpStream) -> Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request = String::new();
        reader.read_line(&mut request)?;
        // Drain the headers; nothing here reads them, but leaving them unread
        // makes some clients see a reset instead of the response.
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                break;
            }
        }

        let target = request.split_whitespace().nth(1).unwrap_or("/");
        match self.resolve(target) {
            Some(path) => {
                let body = std::fs::read(&path)?;
                let mut head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
                    // The same table the public server reads. A preview served
                    // with a different media type is a preview of a different
                    // page — `.wasm` alone decides whether a notebook boots.
                    mecha_manifest::content_type(&path.to_string_lossy()),
                    body.len()
                );
                if !self.without_policy {
                    for (name, value) in self.class.headers() {
                        head.push_str(&format!("{name}: {value}\r\n"));
                    }
                }
                head.push_str("Connection: close\r\n\r\n");
                stream.write_all(head.as_bytes())?;
                stream.write_all(&body)?;
            }
            None => {
                let body = b"not found";
                let head = format!(
                    "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes())?;
                stream.write_all(body)?;
            }
        }
        stream.flush()?;
        Ok(())
    }

    /// Map a request target to a file inside the root, or nothing.
    ///
    /// Containment is proved by canonicalizing and checking the prefix, the
    /// same rule mecha's path jail follows — a preview server is still a server,
    /// and `GET /../../.ssh/id_ed25519` is the first thing anyone tries.
    fn resolve(&self, target: &str) -> Option<PathBuf> {
        let path = target.split(['?', '#']).next().unwrap_or("/");
        let decoded = percent_decode(path);
        let mut candidate = self.root.clone();
        for part in decoded.split('/') {
            if part.is_empty() || part == "." {
                continue;
            }
            // Rejected outright rather than normalised away: a traversal is a
            // request we do not want to serve *any* answer to.
            if part == ".." {
                return None;
            }
            candidate.push(part);
        }
        if candidate.is_dir() {
            candidate.push("index.html");
        }
        let real = candidate.canonicalize().ok()?;
        real.starts_with(&self.root).then_some(real)
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "factory-serve-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// One request against a real socket, returning the raw response.
    fn get(root: &Path, class: ContentClass, target: &str) -> String {
        let preview = Preview::bind(root, class, 0).unwrap();
        let addr = preview.addr().unwrap();
        // Owned, so the client thread borrows nothing from this frame.
        let target = target.to_string();
        let handle = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(addr).unwrap();
            write!(stream, "GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            String::from_utf8_lossy(&response).into_owned()
        });
        preview.serve(1).unwrap();
        handle.join().unwrap()
    }

    #[test]
    fn a_bundle_is_served_with_the_headers_its_class_declares() {
        let s = Scratch::new("headers");
        std::fs::write(s.0.join("index.html"), "<p>hi</p>").unwrap();

        let response = get(&s.0, ContentClass::Compute, "/");
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("script-src 'self' 'wasm-unsafe-eval'"));
        assert!(response.contains("Cross-Origin-Embedder-Policy: require-corp"));
        assert!(response.contains("Content-Type: text/html"));
        assert!(response.ends_with("<p>hi</p>"));

        // A different class is a different policy, from one definition.
        let response = get(&s.0, ContentClass::Static, "/index.html");
        assert!(response.contains("script-src 'none'"));
        assert!(!response.contains("Cross-Origin-Embedder-Policy"));
    }

    /// A preview server is still a server, and this is the first thing anyone
    /// tries.
    #[test]
    fn a_traversal_gets_nothing() {
        let s = Scratch::new("traversal");
        std::fs::create_dir_all(s.0.join("bundle")).unwrap();
        std::fs::write(s.0.join("bundle/index.html"), "inside").unwrap();
        std::fs::write(s.0.join("secret.txt"), "outside").unwrap();
        let root = s.0.join("bundle");

        for target in [
            "/../secret.txt",
            "/..%2Fsecret.txt",
            "/%2e%2e/secret.txt",
            "/a/../../secret.txt",
        ] {
            let response = get(&root, ContentClass::Static, target);
            assert!(
                response.starts_with("HTTP/1.1 404"),
                "{target} was served: {}",
                response.lines().next().unwrap_or("")
            );
            assert!(!response.contains("outside"), "{target} leaked the file");
        }
        // And the ordinary case still works.
        assert!(get(&root, ContentClass::Static, "/").contains("inside"));
    }

    #[test]
    fn wasm_is_served_with_the_type_that_lets_it_stream_compile() {
        let s = Scratch::new("wasm");
        std::fs::write(s.0.join("index.html"), "x").unwrap();
        std::fs::write(s.0.join("m.wasm"), [0x00, 0x61, 0x73, 0x6d]).unwrap();
        let response = get(&s.0, ContentClass::Compute, "/m.wasm");
        // `instantiateStreaming` rejects anything that is not this exact type,
        // which is a real way to spend an afternoon.
        assert!(
            response.contains("Content-Type: application/wasm"),
            "{response:?}"
        );
    }
}
