//! Port 80, which is part of issuance now.
//!
//! One property carries this suite: **a challenge is answered, not
//! redirected.** Let's Encrypt follows redirects, so getting this wrong does
//! not look broken — every name that already holds a certificate keeps
//! renewing, and only a name being issued for the first time fails. That is
//! every signup, and it is the one case nobody exercises by accident.
//!
//! What is *not* here, stated rather than left as a gap: no test drives a
//! token some order is actually waiting on. Challenge data is written by
//! `rustls-acme` through a `pub(crate)` setter, so populating one without
//! talking to an ACME directory is not possible from outside the crate. The
//! answer path is one `find_map` over the resolvers; the routing around it is
//! what breaks, and that is what these cover.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use common::Request;
use mecha_factory::certificates::{Acme, Registry};
use mecha_factory::config::{Config, Limits, Listen, Origins, Tls};
use mecha_factory::db::Db;
use mecha_factory::http::App;

struct Port80 {
    address: SocketAddr,
    _dir: tempfile::TempDir,
}

/// The port-80 listener alone, on a port the kernel chose.
///
/// Real names rather than the loopback authorities the other suites use,
/// because what is under test is the redirect's `Host` resolution and the route
/// that has to win ahead of it.
fn start() -> Port80 {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let address = listener.local_addr().unwrap();

    let config = Config {
        theme: "nocturne".into(),
        mail: None,
        data_dir: dir.path().to_path_buf(),
        origins: Origins {
            gate: "gate.example.org".into(),
            artifacts: "art.example.org".into(),
            compute: "compute.example.org".into(),
        },
        listen: Listen {
            https: ([0, 0, 0, 0], 443).into(),
            http: Some(address),
        },
        // Present so the redirect says `https`, which is the whole point of it.
        tls: Some(Tls {
            contact: "mailto:someone@example.org".into(),
            staging: true,
        }),
        limits: Limits::default(),
        docs_url: None,
        operator_email: None,
        redirect_hosts: vec![],
    };
    let db = Db::open(&config.db_path()).unwrap();
    db.user_create("alice", "alice@example.org", "2026-08-07T00:00:00Z")
        .unwrap();
    let registry = Registry::new(Acme {
        contact: "mailto:someone@example.org".into(),
        staging: true,
        cache: dir.path().join("acme"),
    });
    let app =
        Arc::new(App::with_mailer(config, db, Box::new(common::CapturedMail::default())).unwrap());

    std::thread::spawn(move || {
        runtime
            .block_on(async {
                axum::serve(listener, mecha_factory::tls::port_80(app, registry)).await
            })
            .unwrap();
    });
    std::thread::sleep(std::time::Duration::from_millis(100));

    Port80 { address, _dir: dir }
}

/// The load-bearing one. A challenge for a token nobody is waiting on is a
/// **404**, and specifically not a redirect — a 308 here would mean the
/// fallback swallowed the route, and issuance would fail for exactly the names
/// that have no certificate yet.
#[test]
fn a_challenge_is_answered_and_never_redirected() {
    let server = start();
    for host in [
        "gate.example.org",
        "alice.art.example.org",
        // A name we do not serve. The redirect would 404 this too, so the
        // status alone would not tell them apart — which is why the case below
        // pins the redirect's own behaviour separately.
        "nobody.example.net",
    ] {
        let reply = Request::new(
            "GET",
            "/.well-known/acme-challenge/atokennobodyiswaitingon",
            host,
        )
        .send(server.address);
        assert_eq!(reply.status, 404, "{host}: {}", reply.head);
        assert!(
            reply.header("location").is_none(),
            "{host} was redirected, so the challenge route did not win: {}",
            reply.head
        );
    }
}

/// Port 80 still does what it was built for. A user's plain link has to arrive
/// at *their* name — landing on the bare origin would put every user's links in
/// somebody else's namespace.
#[test]
fn a_bare_hostname_still_redirects_to_its_own_name() {
    let server = start();
    let reply = Request::new("GET", "/b/report/v/1/", "alice.art.example.org").send(server.address);
    assert_eq!(reply.status, 308, "{}", reply.head);
    assert_eq!(
        reply.header("location").as_deref(),
        Some("https://alice.art.example.org/b/report/v/1/")
    );
}

/// A name we do not serve is told nothing and sent nowhere. Redirecting an
/// arbitrary `Host` would make port 80 an open redirector, which is a small
/// thing until this origin hands out capability URLs.
#[test]
fn an_unserved_name_is_not_redirected_anywhere() {
    let server = start();
    let reply = Request::new("GET", "/", "somewhere.else.example.net").send(server.address);
    assert_eq!(reply.status, 404, "{}", reply.head);
    assert!(reply.header("location").is_none(), "{}", reply.head);
}
