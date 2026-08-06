// Each test binary uses a subset of this harness, so an unused helper here is
// not dead code — it is a helper the *other* suite needs.
#![allow(dead_code)]

//! A real server on three real sockets, and a client made of `TcpStream`.
//!
//! The client is hand-rolled on purpose. These suites are mostly about the
//! `Host` header, the response headers, and what happens to a body that is not
//! what it says it is — and a helpful HTTP library would be managing exactly
//! those things on our behalf. Here the bytes on the wire are the test.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

use mecha_factory::config::{Config, Limits, Listen, Origins};
use mecha_factory::db::Db;
use mecha_factory::http::App;

pub struct Server {
    pub gate: SocketAddr,
    pub artifacts: SocketAddr,
    pub compute: SocketAddr,
    pub db: Db,
    pub dir: tempfile::TempDir,
}

/// A response, split just far enough to assert on.
pub struct Reply {
    pub status: u16,
    pub head: String,
    pub body: String,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<String> {
        let name = format!("{}:", name.to_ascii_lowercase());
        self.head
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with(&name))
            .map(|line| line[name.len()..].trim().to_string())
    }

    /// The body as JSON, for the endpoints that answer with it.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", self.body))
    }
}

/// One request, with everything about it under the test's control.
pub struct Request<'a> {
    pub method: &'a str,
    pub target: &'a str,
    pub host: String,
    pub auth: Option<&'a str>,
    pub body: Vec<u8>,
    pub extra: Vec<(String, String)>,
}

impl<'a> Request<'a> {
    pub fn new(method: &'a str, target: &'a str, host: impl Into<String>) -> Request<'a> {
        Request {
            method,
            target,
            host: host.into(),
            auth: None,
            body: Vec::new(),
            extra: Vec::new(),
        }
    }

    pub fn auth(mut self, token: &'a str) -> Self {
        self.auth = Some(token);
        self
    }

    pub fn body(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.body = bytes.into();
        self
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.extra.push((name.to_string(), value.to_string()));
        self
    }

    pub fn send(self, address: SocketAddr) -> Reply {
        let mut stream = TcpStream::connect(address).unwrap();
        let mut head = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
            self.method,
            self.target,
            self.host,
            self.body.len()
        );
        if let Some(token) = self.auth {
            head.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        for (name, value) in &self.extra {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(&self.body).unwrap();
        stream.flush().unwrap();

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let text = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        Reply {
            status,
            head: head.to_string(),
            body: body.to_string(),
        }
    }
}

/// Bind three ports the kernel chose, build the origin table from what we
/// actually got, and serve on a background runtime.
///
/// Port 0 rather than fixed ports, so two tests running at once cannot collide
/// — which is how a suite acquires a flake that reads as a server bug.
pub fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let listeners: Vec<_> = (0..3)
        .map(|_| {
            runtime
                .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                .unwrap()
        })
        .collect();
    let addresses: Vec<SocketAddr> = listeners.iter().map(|l| l.local_addr().unwrap()).collect();

    let config = Config {
        data_dir: dir.path().to_path_buf(),
        origins: Origins {
            gate: addresses[0].to_string(),
            artifacts: addresses[1].to_string(),
            compute: addresses[2].to_string(),
        },
        listen: Listen {
            https: addresses[0],
            http: None,
        },
        tls: None,
        limits: Limits::default(),
    };
    let db = Db::open(&config.db_path()).unwrap();
    let app = Arc::new(App::new(config, db.clone()).unwrap());

    std::thread::spawn(move || {
        runtime
            .block_on(mecha_factory::serve::run_on(app, listeners))
            .unwrap();
    });
    // The listeners are already bound, so a connection cannot be refused; the
    // accept loop just has to get to it.
    std::thread::sleep(std::time::Duration::from_millis(100));

    Server {
        gate: addresses[0],
        artifacts: addresses[1],
        compute: addresses[2],
        db,
        dir,
    }
}

impl Server {
    pub fn get(&self, address: SocketAddr, target: &str) -> Reply {
        Request::new("GET", target, address.to_string()).send(address)
    }

    pub fn key(&self, scope: mecha_factory::db::Scope) -> String {
        mecha_factory::keys::mint(&self.db, scope, "test")
            .unwrap()
            .token
    }
}

/// A bundle as the publisher will send it: a gzipped tar with `bundle.json`
/// inside, carrying the digest computed at home.
pub fn bundle_archive(
    id: &str,
    version: u32,
    class: mecha_manifest::ContentClass,
    body: &str,
) -> Vec<u8> {
    let index = format!("<h1>{body}</h1>");
    let files: Vec<(String, Vec<u8>)> = vec![
        ("index.html".to_string(), index.into_bytes()),
        ("assets/app.wasm".to_string(), vec![0x00, 0x61, 0x73, 0x6d]),
    ];
    let digest =
        mecha_manifest::digest_files(files.iter().map(|(p, b)| (p.as_str(), b.as_slice())));
    let manifest = mecha_manifest::BundleManifest {
        id: id.into(),
        version,
        title: format!("{id} report"),
        description: None,
        template: "report".into(),
        class,
        visibility: mecha_manifest::Visibility::Private,
        digest: Some(digest),
        published_at: Some("2026-08-06T07:00:00Z".into()),
        // What must never reach the box. Deliberately present in the fixture so
        // the test that says so is testing something.
        sources: vec![std::path::PathBuf::from(
            "/home/someone/.mecha/work/morning/2026-08-06.md",
        )],
    };

    let mut all = files;
    all.push(("bundle.json".to_string(), manifest.to_json().into_bytes()));
    tar_gz(&all)
}

pub fn tar_gz(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, bytes) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, &bytes[..]).unwrap();
    }
    let tar = builder.into_inner().unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&tar).unwrap();
    encoder.finish().unwrap()
}
