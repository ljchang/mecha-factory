//! What the world gets back, driven against a real socket.
//!
//! The client is thirty lines of `TcpStream` rather than a library, for one
//! reason: this suite is mostly about the `Host` header and the response
//! headers, and a client that helpfully manages either would be testing itself.
//! Here the exact bytes on the wire are the test.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

use mecha_factory::config::{Config, Limits, Listen, Origins};
use mecha_factory::db::{BundleRow, Db};
use mecha_factory::http::App;
use mecha_manifest::{ContentClass, Visibility};

struct Server {
    gate: SocketAddr,
    artifacts: SocketAddr,
    compute: SocketAddr,
    db: Db,
    _dir: tempfile::TempDir,
}

/// A response, split just far enough to assert on.
struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        let name = format!("{}:", name.to_ascii_lowercase());
        self.head
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with(&name))
            .map(|line| line[name.len()..].trim().to_string())
    }
}

fn get(address: SocketAddr, host: &str, target: &str, auth: Option<&str>) -> Reply {
    let mut stream = TcpStream::connect(address).unwrap();
    let mut request = format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(token) = auth {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
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

/// Bind three ports the kernel chose, build the origin table from what we
/// actually got, and serve on a background runtime.
fn start() -> Server {
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
        _dir: dir,
    }
}

impl Server {
    /// Put a bundle on disk and in the ledger, the way a publish will.
    fn publish(&self, id: &str, version: u32, class: ContentClass, visibility: Visibility) {
        let dir = self
            ._dir
            .path()
            .join("bundles")
            .join(id)
            .join(version.to_string());
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("index.html"), format!("<h1>{id} v{version}</h1>")).unwrap();
        std::fs::write(dir.join("assets/app.wasm"), [0x00, 0x61, 0x73, 0x6d]).unwrap();
        self.db
            .bundle_insert(&BundleRow {
                id: id.into(),
                version,
                digest: format!("sha256:{id}{version}"),
                class,
                title: id.into(),
                description: None,
                template: "report".into(),
                published_at: Some("2026-08-06T07:00:00Z".into()),
                received_at: "2026-08-06T07:00:01Z".into(),
            })
            .unwrap();
        self.db
            .alias_set(id, Some(version), visibility, "2026-08-06T07:00:02Z")
            .unwrap();
    }

    fn host(&self, address: SocketAddr) -> String {
        address.to_string()
    }
}

/// The one that decides everything else: three names, three policies, and
/// nothing served under a name we do not know.
#[test]
fn each_origin_answers_only_for_what_belongs_on_it() {
    let server = start();
    server.publish("brief", 1, ContentClass::Static, Visibility::Public);
    server.publish("nb", 1, ContentClass::Compute, Visibility::Public);

    // A name we do not serve gets nothing, on a socket that serves plenty.
    let reply = get(server.artifacts, "elsewhere.example", "/b/brief/", None);
    assert_eq!(reply.status, 404);

    // The API is the gate's, and only the gate's.
    assert_eq!(
        get(server.gate, &server.host(server.gate), "/v1/health", None).status,
        200
    );
    assert_eq!(
        get(
            server.artifacts,
            &server.host(server.artifacts),
            "/v1/health",
            None
        )
        .status,
        404
    );
    // …and bundles are not the gate's.
    assert_eq!(
        get(server.gate, &server.host(server.gate), "/b/brief/", None).status,
        404
    );

    // A static report, under a policy where nothing runs.
    let reply = get(
        server.artifacts,
        &server.host(server.artifacts),
        "/b/brief/v/1/",
        None,
    );
    assert_eq!(reply.status, 200);
    assert!(reply.body.contains("brief v1"));
    let csp = reply.header("content-security-policy").unwrap();
    assert!(csp.contains("script-src 'none'"), "{csp}");
    assert!(!csp.contains("wasm-unsafe-eval"), "{csp}");
    assert_eq!(
        reply.header("cache-control").unwrap(),
        "public, max-age=31536000, immutable",
        "a version's bytes can never change"
    );
    assert_eq!(reply.header("x-content-type-options").unwrap(), "nosniff");

    // A notebook, on the origin that exists to hold `wasm-unsafe-eval`.
    let reply = get(
        server.compute,
        &server.host(server.compute),
        "/b/nb/v/1/",
        None,
    );
    assert_eq!(reply.status, 200);
    let csp = reply.header("content-security-policy").unwrap();
    assert!(csp.contains("'wasm-unsafe-eval'"), "{csp}");
    assert_eq!(
        reply.header("cross-origin-embedder-policy").unwrap(),
        "require-corp"
    );

    // The same notebook asked for on the artifact origin is sent to the right
    // one rather than served under the wrong policy.
    let reply = get(
        server.artifacts,
        &server.host(server.artifacts),
        "/b/nb/v/1/",
        None,
    );
    assert_eq!(reply.status, 302);
    assert!(
        reply
            .header("location")
            .unwrap()
            .starts_with(&format!("http://{}", server.compute)),
        "{:?}",
        reply.header("location")
    );
}

/// `instantiateStreaming` refuses anything but this type, and the failure reads
/// as a broken notebook rather than as a header problem.
#[test]
fn wasm_is_served_as_wasm() {
    let server = start();
    server.publish("nb", 1, ContentClass::Compute, Visibility::Public);
    let reply = get(
        server.compute,
        &server.host(server.compute),
        "/b/nb/v/1/assets/app.wasm",
        None,
    );
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("content-type").unwrap(), "application/wasm");
}

/// The share URL follows the alias and is never cached; the version URL is
/// forever.
#[test]
fn the_share_url_moves_and_the_version_url_does_not() {
    let server = start();
    server.publish("brief", 1, ContentClass::Static, Visibility::Public);
    server.publish("brief", 2, ContentClass::Static, Visibility::Public);

    let host = server.host(server.artifacts);
    let reply = get(server.artifacts, &host, "/b/brief/", None);
    assert_eq!(reply.status, 302);
    assert_eq!(reply.header("location").unwrap(), "/b/brief/v/2/");
    assert_eq!(reply.header("cache-control").unwrap(), "no-store");

    // Version 1 is still exactly where it was.
    let reply = get(server.artifacts, &host, "/b/brief/v/1/", None);
    assert_eq!(reply.status, 200);
    assert!(reply.body.contains("brief v1"));

    // Moving the alias back moves every share link with it.
    server
        .db
        .alias_set("brief", Some(1), Visibility::Public, "t")
        .unwrap();
    assert_eq!(
        get(server.artifacts, &host, "/b/brief/", None)
            .header("location")
            .unwrap(),
        "/b/brief/v/1/"
    );
}

/// Visibility is enforced here for the first time — it was recorded and
/// unenforced while there was no origin — and it fails closed in every
/// direction.
#[test]
fn a_private_bundle_is_indistinguishable_from_one_that_never_existed() {
    let server = start();
    server.publish("secret", 1, ContentClass::Static, Visibility::Private);
    let host = server.host(server.artifacts);

    let private = get(server.artifacts, &host, "/b/secret/", None);
    let absent = get(server.artifacts, &host, "/b/nothing/", None);
    assert_eq!(private.status, 404);
    assert_eq!(absent.status, 404);
    assert_eq!(private.body, absent.body, "the difference is the answer");
    // The version URL is not a way around it.
    assert_eq!(
        get(server.artifacts, &host, "/b/secret/v/1/", None).status,
        404
    );

    // A bundle published but never aliased is not yet a publication.
    server
        .db
        .bundle_insert(&BundleRow {
            id: "unaliased".into(),
            version: 1,
            digest: "sha256:x".into(),
            class: ContentClass::Static,
            title: "t".into(),
            description: None,
            template: "report".into(),
            published_at: None,
            received_at: "t".into(),
        })
        .unwrap();
    assert_eq!(
        get(server.artifacts, &host, "/b/unaliased/v/1/", None).status,
        404
    );
}

/// A takedown covers the version URLs too, or it is a suggestion — and it says
/// "this was here", because the reader followed a link somebody sent them.
#[test]
fn a_takedown_is_gone_everywhere_and_says_so() {
    let server = start();
    server.publish("brief", 1, ContentClass::Static, Visibility::Public);
    let host = server.host(server.artifacts);
    server
        .db
        .alias_set("brief", None, Visibility::Public, "t")
        .unwrap();

    for target in ["/b/brief/", "/b/brief/v/1/", "/b/brief/v/1/index.html"] {
        let reply = get(server.artifacts, &host, target, None);
        assert_eq!(reply.status, 410, "{target}");
        assert!(reply.body.contains("taken down"), "{target}");
    }
}

/// The first thing anyone tries, against a server that really does have
/// something to steal one directory up.
#[test]
fn nothing_escapes_a_bundle() {
    let server = start();
    server.publish("brief", 1, ContentClass::Static, Visibility::Public);
    server.publish("other", 1, ContentClass::Static, Visibility::Public);
    let host = server.host(server.artifacts);

    for target in [
        "/b/brief/v/1/../../../../factory.db",
        "/b/brief/v/1/..%2f..%2f..%2ffactory.db",
        "/b/brief/v/1/%2e%2e/%2e%2e/other/1/index.html",
        "/b/brief/v/1/./../../other/1/index.html",
    ] {
        let reply = get(server.artifacts, &host, target, None);
        assert!(
            reply.status == 404 || reply.status == 400,
            "{target} → {} {}",
            reply.status,
            reply.body
        );
        assert!(!reply.body.contains("other v1"), "{target} leaked");
    }
    // Not vacuous: the file it was reaching for is really there, and the
    // ordinary case still works.
    assert!(server._dir.path().join("factory.db").is_file());
    assert_eq!(
        get(server.artifacts, &host, "/b/brief/v/1/index.html", None).status,
        200
    );
}

/// Public enough to watch from a trigger with no key; private about how many
/// strangers wrote to us this week.
#[test]
fn health_is_public_and_the_counts_are_not() {
    let server = start();
    server.publish("brief", 1, ContentClass::Static, Visibility::Public);
    let host = server.host(server.gate);

    let reply = get(server.gate, &host, "/v1/health", None);
    assert_eq!(reply.status, 200);
    assert!(reply.body.contains("\"status\":\"ok\""), "{}", reply.body);
    assert!(!reply.body.contains("bundles"), "{}", reply.body);

    let minted =
        mecha_factory::keys::mint(&server.db, mecha_factory::db::Scope::Drain, "trigger").unwrap();
    let reply = get(server.gate, &host, "/v1/health", Some(&minted.token));
    assert!(reply.body.contains("\"bundles\":1"), "{}", reply.body);
    assert!(reply.body.contains("\"queued\":0"), "{}", reply.body);

    // A wrong token gets the public answer rather than a refusal: health must
    // work on a box where every key has just been rotated.
    let reply = get(server.gate, &host, "/v1/health", Some("mk_pub_aa.bb"));
    assert_eq!(reply.status, 200);
    assert!(!reply.body.contains("bundles"));
}
