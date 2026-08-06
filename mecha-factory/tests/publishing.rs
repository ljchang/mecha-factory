//! The write half: a bundle crossing the wire, a type being uploaded, and a
//! queue being drained — against a real server, with real keys.

mod common;

use std::io::Read;

use common::{bundle_archive, start, tar_gz, Request};
use mecha_factory::db::Scope;
use mecha_manifest::ContentClass;

/// The whole point, in one test: an agent publishes, a human releases, and a
/// stranger with the link reads it.
#[test]
fn a_bundle_published_and_then_aliased_is_a_page_on_the_internet() {
    let server = start();
    let gate = server.gate.to_string();
    let key = server.key(Scope::Publish);

    let reply = Request::new("POST", "/v1/bundles", &gate)
        .auth(&key)
        .body(bundle_archive("brief", 1, ContentClass::Static, "Monday"))
        .send(server.gate);
    assert_eq!(reply.status, 201, "{}", reply.body);
    let published = reply.json();
    assert_eq!(published["version"], 1);
    assert_eq!(published["existing"], false);
    assert_eq!(
        published["url"],
        format!("http://alice.{}/b/brief/", server.artifacts),
        "a published URL names its owner, from the first one"
    );

    // Published is not yet readable: the alias has not moved and nothing is
    // public.
    assert_eq!(server.get(server.artifacts, "/b/brief/").status, 404);

    let reply = Request::new("POST", "/v1/bundles/brief/alias", &gate)
        .auth(&key)
        .body(r#"{"version":1,"visibility":"public"}"#)
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);

    let reply = server.get(server.artifacts, "/b/brief/");
    assert_eq!(reply.status, 302);
    assert_eq!(reply.header("location").unwrap(), "/b/brief/v/1/");
    let reply = server.get(server.artifacts, "/b/brief/v/1/");
    assert_eq!(reply.status, 200);
    assert!(reply.body.contains("Monday"), "{}", reply.body);
    assert!(reply
        .header("content-security-policy")
        .unwrap()
        .contains("script-src 'none'"));
}

/// The one that would have published the shape of a private machine, and that
/// nothing would have noticed — because `bundle.json` is itself served.
#[test]
fn the_home_paths_in_the_manifest_do_not_reach_the_box() {
    let server = start();
    let gate = server.gate.to_string();
    let key = server.key(Scope::Publish);
    Request::new("POST", "/v1/bundles", &gate)
        .auth(&key)
        .body(bundle_archive("brief", 1, ContentClass::Static, "Monday"))
        .send(server.gate);
    Request::new("POST", "/v1/bundles/brief/alias", &gate)
        .auth(&key)
        .body(r#"{"version":1,"visibility":"public"}"#)
        .send(server.gate);

    let served = server.get(server.artifacts, "/b/brief/v/1/bundle.json");
    assert_eq!(served.status, 200);
    assert!(
        !served.body.contains("/home/someone"),
        "the box is serving home's paths: {}",
        served.body
    );
    assert!(served.body.contains("\"id\": \"brief\""));

    // Not vacuous: the fixture really does carry them.
    let archive = bundle_archive("x", 1, ContentClass::Static, "x");
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(&archive[..])
        .read_to_end(&mut raw)
        .unwrap();
    assert!(
        String::from_utf8_lossy(&raw).contains("/home/someone"),
        "the fixture is honest"
    );
}

/// Content addressing is what makes a retry free — and the outbox stages with
/// no lock, deliberately, so a duplicate publish is a thing that happens.
#[test]
fn identical_bytes_mint_nothing_and_a_changed_version_is_refused() {
    let server = start();
    let gate = server.gate.to_string();
    let key = server.key(Scope::Publish);
    let archive = bundle_archive("brief", 1, ContentClass::Static, "Monday");

    let first = Request::new("POST", "/v1/bundles", &gate)
        .auth(&key)
        .body(archive.clone())
        .send(server.gate);
    assert_eq!(first.status, 201);
    let original = server
        .db
        .bundle(&server.user.id, "brief", 1)
        .unwrap()
        .unwrap()
        .digest;

    let again = Request::new("POST", "/v1/bundles", &gate)
        .auth(&key)
        .body(archive)
        .send(server.gate);
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(again.json()["existing"], true);
    assert_eq!(again.json()["version"], 1);
    assert_eq!(
        server.db.bundle_versions(&server.user.id, "brief").unwrap(),
        vec![1]
    );

    // Version 1 with different bytes: refused, and version 1 is untouched.
    let conflict = Request::new("POST", "/v1/bundles", &gate)
        .auth(&key)
        .body(bundle_archive("brief", 1, ContentClass::Static, "Tuesday"))
        .send(server.gate);
    assert_eq!(conflict.status, 409, "{}", conflict.body);
    assert!(conflict.body.contains("written once"), "{}", conflict.body);
    assert_eq!(
        server
            .db
            .bundle(&server.user.id, "brief", 1)
            .unwrap()
            .unwrap()
            .digest,
        original,
        "the refused publish changed the version it collided with"
    );

    // The next version is fine, and both exist.
    let second = Request::new("POST", "/v1/bundles", &gate)
        .auth(&key)
        .body(bundle_archive("brief", 2, ContentClass::Static, "Tuesday"))
        .send(server.gate);
    assert_eq!(second.status, 201, "{}", second.body);
    assert_eq!(
        server.db.bundle_versions(&server.user.id, "brief").unwrap(),
        vec![1, 2]
    );
}

/// A retry after a timeout may be re-rendering rather than re-sending, and two
/// renders of the same report are not always byte-identical.
#[test]
fn an_idempotency_key_returns_the_publish_that_already_landed() {
    let server = start();
    let gate = server.gate.to_string();
    let key = server.key(Scope::Publish);

    let first = Request::new("POST", "/v1/bundles", &gate)
        .auth(&key)
        .header("Idempotency-Key", "abc-123")
        .body(bundle_archive("brief", 1, ContentClass::Static, "Monday"))
        .send(server.gate);
    assert_eq!(first.status, 201);

    // Different bytes, same key: the original version, and nothing minted.
    let retry = Request::new("POST", "/v1/bundles", &gate)
        .auth(&key)
        .header("Idempotency-Key", "abc-123")
        .body(bundle_archive("brief", 2, ContentClass::Static, "Tuesday"))
        .send(server.gate);
    assert_eq!(retry.status, 200, "{}", retry.body);
    assert_eq!(retry.json()["version"], 1);
    assert_eq!(retry.json()["existing"], true);
    assert_eq!(
        server.db.bundle_versions(&server.user.id, "brief").unwrap(),
        vec![1]
    );
}

/// Two keys, two scopes, and neither does the other's work.
#[test]
fn a_key_reaches_exactly_what_its_scope_names() {
    let server = start();
    let gate = server.gate.to_string();
    let publish = server.key(Scope::Publish);
    let drain = server.key(Scope::Drain);
    let archive = || bundle_archive("brief", 1, ContentClass::Static, "Monday");

    // No key at all.
    assert_eq!(
        Request::new("POST", "/v1/bundles", &gate)
            .body(archive())
            .send(server.gate)
            .status,
        401
    );
    // The wrong key: 403, because "you, but not here" is a different problem
    // from "who are you" for a client deciding whether to try another.
    assert_eq!(
        Request::new("POST", "/v1/bundles", &gate)
            .auth(&drain)
            .body(archive())
            .send(server.gate)
            .status,
        403
    );
    assert_eq!(
        Request::new("GET", "/v1/queue", &gate)
            .auth(&publish)
            .send(server.gate)
            .status,
        403
    );
    // And the right ones work.
    assert_eq!(
        Request::new("POST", "/v1/bundles", &gate)
            .auth(&publish)
            .body(archive())
            .send(server.gate)
            .status,
        201
    );
    assert_eq!(
        Request::new("GET", "/v1/queue", &gate)
            .auth(&drain)
            .send(server.gate)
            .status,
        200
    );

    // Nothing is published from the artifact origin, whatever the key says.
    assert_eq!(
        Request::new("POST", "/v1/bundles", server.artifacts.to_string())
            .auth(&publish)
            .body(archive())
            .send(server.artifacts)
            .status,
        404
    );
}

/// Everything the unpacker refuses, refused across the wire with a message the
/// publisher can act on.
#[test]
fn a_bundle_that_is_not_what_it_claims_is_refused_with_a_reason() {
    let server = start();
    let gate = server.gate.to_string();
    let key = server.key(Scope::Publish);

    // A manifest whose digest does not match the bytes.
    let mut files: Vec<(String, Vec<u8>)> = vec![("index.html".into(), b"<h1>real</h1>".to_vec())];
    let manifest = mecha_manifest::BundleManifest {
        id: "brief".into(),
        version: 1,
        title: "t".into(),
        description: None,
        template: "report".into(),
        class: ContentClass::Static,
        visibility: mecha_manifest::Visibility::Private,
        digest: Some("sha256:0000".into()),
        published_at: None,
        sources: vec![],
    };
    files.push(("bundle.json".into(), manifest.to_json().into_bytes()));
    let reply = Request::new("POST", "/v1/bundles", &gate)
        .auth(&key)
        .body(tar_gz(&files))
        .send(server.gate);
    assert_eq!(reply.status, 400);
    assert!(
        reply.body.contains("not what was reviewed"),
        "{}",
        reply.body
    );

    // Not an archive at all.
    let reply = Request::new("POST", "/v1/bundles", &gate)
        .auth(&key)
        .body("hello")
        .send(server.gate);
    assert_eq!(reply.status, 400);

    // Nothing was stored by any of it.
    assert_eq!(server.db.bundle_count().unwrap(), 0);
    assert!(!server.dir.path().join("bundles/brief").exists());
}

/// One file in, a schema out — and the server derives the schema rather than
/// accepting one, so the form, the schema and the validator cannot disagree.
#[test]
fn a_request_type_is_uploaded_once_and_readable_by_anyone() {
    let server = start();
    let gate = server.gate.to_string();
    let key = server.key(Scope::Publish);
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../mecha-manifest/types/meeting.toml"),
    )
    .unwrap();

    let reply = Request::new("PUT", "/v1/types/meeting", &gate)
        .auth(&key)
        .body(manifest.clone())
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.json()["fields"].as_u64().unwrap() > 0);

    // Discovery is per-user, so it is authenticated: an agent learns the shape
    // of every request *it* could make. An anonymous listing would have been a
    // list of every user's forms, which is the one place tenancy would have
    // silently not held.
    let reply = Request::new("GET", "/v1/types", &gate)
        .auth(&key)
        .send(server.gate);
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["types"][0]["id"], "meeting");

    let reply = Request::new("GET", "/v1/types/meeting", &gate)
        .auth(&key)
        .send(server.gate);
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["schema"]["type"], "object");

    assert_eq!(server.get(server.gate, "/v1/types").status, 401);

    // Uploading is not public either.
    assert_eq!(
        Request::new("PUT", "/v1/types/meeting", &gate)
            .body(manifest.clone())
            .send(server.gate)
            .status,
        401
    );

    // An id is a path segment, a filename and a tool name, so it cannot be two
    // things.
    let reply = Request::new("PUT", "/v1/types/somethingelse", &gate)
        .auth(&key)
        .body(manifest)
        .send(server.gate);
    assert_eq!(reply.status, 400);
    assert!(
        reply.body.contains("cannot be two things"),
        "{}",
        reply.body
    );
}

/// Draining is a read; acknowledging is the delete. The record survives every
/// failure except home saying it has it.
#[test]
fn a_record_survives_until_home_says_it_has_it() {
    let server = start();
    let gate = server.gate.to_string();
    let drain = server.key(Scope::Drain);

    let a = server
        .db
        .queue_add(
            &server.user.id,
            "meeting",
            "queued",
            r#"{"name":"A"}"#,
            "t1",
            None,
        )
        .unwrap();
    let b = server
        .db
        .queue_add(
            &server.user.id,
            "meeting",
            "queued",
            r#"{"name":"B"}"#,
            "t2",
            None,
        )
        .unwrap();
    // Submitted but never verified: never drained, and it never costs a token.
    server
        .db
        .queue_add(
            &server.user.id,
            "meeting",
            "submitted",
            r#"{"name":"C"}"#,
            "t3",
            None,
        )
        .unwrap();

    let reply = Request::new("GET", "/v1/queue", &gate)
        .auth(&drain)
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let records = reply.json();
    assert_eq!(records["records"].as_array().unwrap().len(), 2);
    assert_eq!(records["records"][0]["seq"], a);
    assert_eq!(records["next"], b);
    assert!(
        !reply.body.contains("\"C\""),
        "an unverified record drained"
    );

    // A drain that never reached home changed nothing.
    let again = Request::new("GET", "/v1/queue", &gate)
        .auth(&drain)
        .send(server.gate);
    assert_eq!(again.json()["records"].as_array().unwrap().len(), 2);

    let reply = Request::new("POST", "/v1/queue/ack", &gate)
        .auth(&drain)
        .body(format!(r#"{{"seqs":[{a}]}}"#))
        .send(server.gate);
    assert_eq!(reply.json()["deleted"], 1);

    let after = Request::new("GET", "/v1/queue", &gate)
        .auth(&drain)
        .send(server.gate);
    let records = after.json();
    assert_eq!(records["records"].as_array().unwrap().len(), 1);
    assert_eq!(records["records"][0]["seq"], b);

    // The watermark form of the same question.
    let empty = Request::new("GET", &format!("/v1/queue?since={b}"), &gate)
        .auth(&drain)
        .send(server.gate);
    assert!(empty.json()["records"].as_array().unwrap().is_empty());
}
