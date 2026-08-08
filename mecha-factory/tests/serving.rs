//! What the world gets back, driven against a real socket.
//!
//! The client is thirty lines of `TcpStream` rather than a library, for one
//! reason: this suite is mostly about the `Host` header and the response
//! headers, and a client that helpfully manages either would be testing itself.
//! Here the exact bytes on the wire are the test.

mod common;

use common::{start, Request, Server};
use mecha_factory::db::BundleRow;
use mecha_manifest::{ContentClass, Visibility};
use std::net::SocketAddr;

/// Put a bundle on disk and in the ledger, the way a publish does.
fn publish(server: &Server, id: &str, version: u32, class: ContentClass, visibility: Visibility) {
    publish_as(server, &server.user.clone(), id, version, class, visibility)
}

fn publish_as(
    server: &Server,
    user: &mecha_factory::db::UserRow,
    id: &str,
    version: u32,
    class: ContentClass,
    visibility: Visibility,
) {
    let dir = server
        .dir
        .path()
        .join("bundles")
        .join(&user.id)
        .join(id)
        .join(version.to_string());
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(dir.join("index.html"), format!("<h1>{id} v{version}</h1>")).unwrap();
    std::fs::write(dir.join("assets/app.wasm"), [0x00, 0x61, 0x73, 0x6d]).unwrap();
    server
        .db
        .bundle_insert(&BundleRow {
            user_id: user.id.clone(),
            id: id.into(),
            version,
            digest: format!("sha256:{id}{version}"),
            class,
            title: id.into(),
            description: None,
            template: "report".into(),
            published_at: Some("2026-08-06T07:00:00Z".into()),
            received_at: "2026-08-06T07:00:01Z".into(),
            withheld_at: None,
            withheld_reason: None,
        })
        .unwrap();
    server
        .db
        .alias_set(
            &user.id,
            id,
            Some(version),
            visibility,
            "2026-08-06T07:00:02Z",
        )
        .unwrap();
}

/// A request under a `Host` the test chooses, which is the whole subject here.
fn get_host(address: SocketAddr, host: &str, target: &str, auth: Option<&str>) -> common::Reply {
    let mut request = Request::new("GET", target, host);
    if let Some(token) = auth {
        request = request.auth(token);
    }
    request.send(address)
}

/// The one that decides everything else: three names, three policies, and
/// nothing served under a name we do not know.
#[test]
fn each_origin_answers_only_for_what_belongs_on_it() {
    let server = start();
    publish(
        &server,
        "brief",
        1,
        ContentClass::Static,
        Visibility::Public,
    );
    publish(&server, "nb", 1, ContentClass::Compute, Visibility::Public);

    // A name we do not serve gets nothing, on a socket that serves plenty.
    let reply = get_host(server.artifacts, "elsewhere.example", "/b/brief/", None);
    assert_eq!(reply.status, 404);

    // The API is the gate's, and only the gate's.
    assert_eq!(
        get_host(server.gate, &server.gate.to_string(), "/v1/health", None).status,
        200
    );
    assert_eq!(
        get_host(
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
        get_host(server.gate, &server.gate.to_string(), "/b/brief/", None).status,
        404
    );

    // A static report, under a policy where nothing runs.
    let reply = get_host(
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
    let reply = get_host(
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
    let reply = get_host(
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
            .starts_with(&format!("http://alice.{}", server.compute)),
        "{:?}",
        reply.header("location")
    );
}

/// `instantiateStreaming` refuses anything but this type, and the failure reads
/// as a broken notebook rather than as a header problem.
#[test]
fn wasm_is_served_as_wasm() {
    let server = start();
    publish(&server, "nb", 1, ContentClass::Compute, Visibility::Public);
    let reply = get_host(
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
    publish(
        &server,
        "brief",
        1,
        ContentClass::Static,
        Visibility::Public,
    );
    publish(
        &server,
        "brief",
        2,
        ContentClass::Static,
        Visibility::Public,
    );

    let host = server.host(server.artifacts);
    let reply = get_host(server.artifacts, &host, "/b/brief/", None);
    assert_eq!(reply.status, 302);
    assert_eq!(reply.header("location").unwrap(), "/b/brief/v/2/");
    assert_eq!(reply.header("cache-control").unwrap(), "no-store");

    // Version 1 is still exactly where it was.
    let reply = get_host(server.artifacts, &host, "/b/brief/v/1/", None);
    assert_eq!(reply.status, 200);
    assert!(reply.body.contains("brief v1"));

    // Moving the alias back moves every share link with it.
    server
        .db
        .alias_set(&server.user.id, "brief", Some(1), Visibility::Public, "t")
        .unwrap();
    assert_eq!(
        get_host(server.artifacts, &host, "/b/brief/", None)
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
    publish(
        &server,
        "secret",
        1,
        ContentClass::Static,
        Visibility::Private,
    );
    let host = server.host(server.artifacts);

    let private = get_host(server.artifacts, &host, "/b/secret/", None);
    let absent = get_host(server.artifacts, &host, "/b/nothing/", None);
    assert_eq!(private.status, 404);
    assert_eq!(absent.status, 404);
    assert_eq!(private.body, absent.body, "the difference is the answer");
    // The version URL is not a way around it.
    assert_eq!(
        get_host(server.artifacts, &host, "/b/secret/v/1/", None).status,
        404
    );

    // A bundle published but never aliased is not yet a publication.
    server
        .db
        .bundle_insert(&BundleRow {
            user_id: server.user.id.clone(),
            id: "unaliased".into(),
            version: 1,
            digest: "sha256:x".into(),
            class: ContentClass::Static,
            title: "t".into(),
            description: None,
            template: "report".into(),
            published_at: None,
            received_at: "t".into(),
            withheld_at: None,
            withheld_reason: None,
        })
        .unwrap();
    assert_eq!(
        get_host(server.artifacts, &host, "/b/unaliased/v/1/", None).status,
        404
    );
}

/// A takedown covers the version URLs too, or it is a suggestion — and it says
/// "this was here", because the reader followed a link somebody sent them.
#[test]
fn a_takedown_is_gone_everywhere_and_says_so() {
    let server = start();
    publish(
        &server,
        "brief",
        1,
        ContentClass::Static,
        Visibility::Public,
    );
    let host = server.host(server.artifacts);
    server
        .db
        .alias_set(&server.user.id, "brief", None, Visibility::Public, "t")
        .unwrap();

    for target in ["/b/brief/", "/b/brief/v/1/", "/b/brief/v/1/index.html"] {
        let reply = get_host(server.artifacts, &host, target, None);
        assert_eq!(reply.status, 410, "{target}");
        assert!(reply.body.contains("taken down"), "{target}");
    }

    // A bundle that was never public gets no such courtesy: taking it down
    // answers exactly what a bundle that never existed answers, so nothing
    // here is an oracle for what was once on this box.
    publish(
        &server,
        "secret",
        1,
        ContentClass::Static,
        Visibility::Private,
    );
    server
        .db
        .alias_set(&server.user.id, "secret", None, Visibility::Private, "t")
        .unwrap();
    let reply = get_host(server.artifacts, &host, "/b/secret/", None);
    assert_eq!(reply.status, 404);
    assert!(!reply.body.contains("taken down"));
}

/// The first thing anyone tries, against a server that really does have
/// something to steal one directory up.
#[test]
fn nothing_escapes_a_bundle() {
    let server = start();
    publish(
        &server,
        "brief",
        1,
        ContentClass::Static,
        Visibility::Public,
    );
    publish(
        &server,
        "other",
        1,
        ContentClass::Static,
        Visibility::Public,
    );
    let host = server.host(server.artifacts);

    for target in [
        "/b/brief/v/1/../../../../factory.db",
        "/b/brief/v/1/..%2f..%2f..%2ffactory.db",
        "/b/brief/v/1/%2e%2e/%2e%2e/other/1/index.html",
        "/b/brief/v/1/./../../other/1/index.html",
    ] {
        let reply = get_host(server.artifacts, &host, target, None);
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
    assert!(server.dir.path().join("factory.db").is_file());
    assert_eq!(
        get_host(server.artifacts, &host, "/b/brief/v/1/index.html", None).status,
        200
    );
}

/// Public enough to watch from a trigger with no key; private about how many
/// strangers wrote to us this week.
#[test]
fn health_is_public_and_the_counts_are_not() {
    let server = start();
    publish(
        &server,
        "brief",
        1,
        ContentClass::Static,
        Visibility::Public,
    );
    let host = server.gate.to_string();

    let reply = get_host(server.gate, &host, "/v1/health", None);
    assert_eq!(reply.status, 200);
    assert!(reply.body.contains("\"status\":\"ok\""), "{}", reply.body);
    assert!(!reply.body.contains("queued"), "{}", reply.body);

    let minted = mecha_factory::keys::mint(
        &server.db,
        &server.user.id,
        mecha_factory::db::Scope::Drain,
        "trigger",
    )
    .unwrap();
    // What a key gets is *its own user's* state — not the box's. How many
    // reports somebody else published is not a fact this endpoint owes anyone,
    // and a health check that leaked it would be the one place tenancy quietly
    // did not hold.
    let reply = get_host(server.gate, &host, "/v1/health", Some(&minted.token));
    assert!(
        reply.body.contains("\"handle\":\"alice\""),
        "{}",
        reply.body
    );
    assert!(reply.body.contains("\"queued\":0"), "{}", reply.body);
    assert!(!reply.body.contains("bundles"), "{}", reply.body);

    // A wrong token gets the public answer rather than a refusal: health must
    // work on a box where every key has just been rotated.
    let reply = get_host(server.gate, &host, "/v1/health", Some("mk_pub_aa.bb"));
    assert_eq!(reply.status, 200);
    assert!(!reply.body.contains("handle"));
}

/// The version switcher at the bare `v/`: every version linked, the live one
/// named, and the same one-answer rule as the bytes.
#[test]
fn the_version_index_lists_what_a_reader_may_switch_between() {
    let server = start();
    publish(
        &server,
        "switchable",
        1,
        ContentClass::Static,
        Visibility::Public,
    );
    publish(
        &server,
        "switchable",
        2,
        ContentClass::Static,
        Visibility::Public,
    );
    // Pin the share URL to v1 so "live" and "latest" differ.
    server
        .db
        .alias_set(
            &server.user.id,
            "switchable",
            Some(1),
            Visibility::Public,
            "t",
        )
        .unwrap();
    let host = server.host(server.artifacts);

    let index = get_host(server.artifacts, &host, "/b/switchable/v/", None);
    assert_eq!(index.status, 200, "{}", index.body);
    assert!(index.body.contains("/view/switchable/1"), "{}", index.body);
    assert!(index.body.contains("/b/switchable/v/1/"), "{}", index.body);
    assert!(index.body.contains("/b/switchable/v/2/"), "{}", index.body);
    assert!(
        index.body.contains("\u{2014} what the share URL shows"),
        "{}",
        index.body
    );
    assert_eq!(index.header("cache-control").unwrap(), "no-store");

    // The artifact-origin viewer spelling redirects to the one real viewer
    // on the gate — where the session lives — under the same one-answer rule.
    let viewer = get_host(server.artifacts, &host, "/view/switchable/2", None);
    assert_eq!(viewer.status, 302, "{}", viewer.body);
    let target = viewer.header("location").unwrap();
    assert!(target.ends_with("/view/alice/switchable/2"), "{target}");

    // The gate viewer: real chrome, the bundle framed cross-origin from its
    // immutable URL, anonymous gets the sign-in corner and no controls.
    let gate_view = get_host(
        server.gate,
        &server.gate.to_string(),
        "/view/alice/switchable/2",
        None,
    );
    assert_eq!(gate_view.status, 200, "{}", gate_view.body);
    assert!(
        gate_view.body.contains("/b/switchable/v/2/"),
        "{}",
        gate_view.body
    );
    assert!(gate_view.body.contains("<summary>Sign in</summary>"));
    assert!(!gate_view.body.contains("<summary>Manage</summary>"));
    assert!(
        gate_view.body.contains("/view/alice/switchable/1"),
        "switcher"
    );
    let view_csp = gate_view.header("content-security-policy").unwrap();
    assert!(view_csp.contains("frame-src http://"), "{view_csp}");
    assert!(view_csp.contains("frame-ancestors 'none'"), "{view_csp}");

    // And the bundle now names the gate as its one extra permitted framer.
    let bundle = get_host(server.artifacts, &host, "/b/switchable/v/2/", None);
    let bundle_csp = bundle.header("content-security-policy").unwrap();
    assert!(
        bundle_csp.contains("frame-ancestors 'self' http://"),
        "{bundle_csp}"
    );

    // Private: index and viewer answer exactly like a bundle that never
    // existed.
    server
        .db
        .alias_set(
            &server.user.id,
            "switchable",
            Some(1),
            Visibility::Private,
            "t",
        )
        .unwrap();
    let hidden = get_host(server.artifacts, &host, "/b/switchable/v/", None);
    let absent = get_host(server.artifacts, &host, "/b/no-such-bundle/v/", None);
    assert_eq!(hidden.status, 404);
    assert_eq!(hidden.body, absent.body);
    let hidden_view = get_host(server.artifacts, &host, "/view/switchable/1", None);
    assert_eq!(hidden_view.status, 404);
}
