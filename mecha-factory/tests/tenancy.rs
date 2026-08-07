//! Two users on one box, and the boundary between them.
//!
//! Every test here is a question of the form "can Alice reach Bob's ___", and
//! the answer has to be no through every door — the API, the origin, the
//! queue, and the URL space. Isolation that holds in four places and leaks in
//! the fifth is not isolation, so the fifth is what these look for.

mod common;

use common::{bundle_archive, start, Request};
use mecha_factory::db::Scope;
use mecha_manifest::{ContentClass, Visibility};

/// The whole point: the same bundle id, published by two people, is two
/// different pages and neither can address the other's.
#[test]
fn two_users_may_hold_the_same_bundle_id() {
    let server = start();
    let bob = server.add_user("bob");
    let gate = server.gate.to_string();
    let alice_key = server.key(Scope::Publish);
    let alice_key_rel = server.key(Scope::Release);
    let bob_key = server.key_for(&bob, Scope::Publish);

    let bob_key_rel = server.key_for(&bob, Scope::Release);

    // Two keys each: writing a version and making it readable are different
    // scopes now, so a test that does both has to hold both.
    for (key, release, body) in [
        (&alice_key, &alice_key_rel, "Alice's Monday"),
        (&bob_key, &bob_key_rel, "Bob's Monday"),
    ] {
        let reply = Request::new("POST", "/v1/bundles", &gate)
            .auth(key)
            .body(bundle_archive("brief", 1, ContentClass::Static, body))
            .send(server.gate);
        assert_eq!(reply.status, 201, "{}", reply.body);
        let alias = Request::new("POST", "/v1/bundles/brief/alias", &gate)
            .auth(release)
            .body(r#"{"version":1,"visibility":"public"}"#)
            .send(server.gate);
        assert_eq!(alias.status, 200, "{}", alias.body);
    }

    // Same path, two hostnames, two pages.
    let a = server.get_as(server.artifacts, "alice", "/b/brief/v/1/");
    let b = server.get_as(server.artifacts, "bob", "/b/brief/v/1/");
    assert_eq!(a.status, 200);
    assert_eq!(b.status, 200);
    assert!(a.body.contains("Alice's Monday"), "{}", a.body);
    assert!(b.body.contains("Bob's Monday"), "{}", b.body);

    // And they are separate rows and separate bytes on disk, not one shared by
    // a digest match.
    assert_eq!(server.db.bundle_count().unwrap(), 2);
}

/// Publishing under somebody else's name is not a thing a key can do, because
/// the key names the namespace and the request does not.
#[test]
fn a_key_cannot_publish_into_another_users_namespace() {
    let server = start();
    let bob = server.add_user("bob");
    let gate = server.gate.to_string();
    let bob_key = server.key_for(&bob, Scope::Publish);
    let bob_key_rel = server.key_for(&bob, Scope::Release);

    // Bob publishes; the URL that comes back is Bob's, whatever he asked for.
    let reply = Request::new("POST", "/v1/bundles", &gate)
        .auth(&bob_key)
        .body(bundle_archive("brief", 1, ContentClass::Static, "Bob"))
        .send(server.gate);
    assert_eq!(reply.status, 201);
    assert!(
        reply.json()["url"]
            .as_str()
            .unwrap()
            .starts_with(&format!("http://bob.{}", server.artifacts)),
        "{}",
        reply.body
    );
    assert!(server
        .db
        .bundle(&server.user.id, "brief", 1)
        .unwrap()
        .is_none());

    // Bob's key cannot move Alice's alias either — to his key, her bundle
    // simply does not exist.
    server
        .db
        .alias_set(&server.user.id, "hers", None, Visibility::Public, "t")
        .unwrap();
    let reply = Request::new("POST", "/v1/bundles/hers/alias", &gate)
        .auth(&bob_key_rel)
        .body(r#"{"version":1}"#)
        .send(server.gate);
    assert_eq!(reply.status, 404, "{}", reply.body);
}

/// A queue is a user's own. Draining somebody else's would be reading their
/// mail; acknowledging it would be deleting it.
#[test]
fn the_queue_is_per_user_in_both_directions() {
    let server = start();
    let bob = server.add_user("bob");
    let gate = server.gate.to_string();
    let alice_drain = server.key(Scope::Drain);
    let bob_drain = server.key_for(&bob, Scope::Drain);

    let hers = server
        .db
        .queue_add(
            &server.user.id,
            "meeting",
            "queued",
            r#"{"who":"alice"}"#,
            "t1",
            None,
        )
        .unwrap();
    let his = server
        .db
        .queue_add(&bob.id, "meeting", "queued", r#"{"who":"bob"}"#, "t2", None)
        .unwrap();

    let reply = Request::new("GET", "/v1/queue", &gate)
        .auth(&bob_drain)
        .send(server.gate);
    let records = reply.json();
    assert_eq!(records["records"].as_array().unwrap().len(), 1);
    assert_eq!(records["records"][0]["seq"], his);
    assert!(!reply.body.contains("alice"), "{}", reply.body);

    // Acknowledging a sequence number he can see the shape of but does not own
    // deletes nothing.
    let reply = Request::new("POST", "/v1/queue/ack", &gate)
        .auth(&bob_drain)
        .body(format!(r#"{{"seqs":[{hers}]}}"#))
        .send(server.gate);
    assert_eq!(reply.json()["deleted"], 0, "{}", reply.body);

    let still_there = Request::new("GET", "/v1/queue", &gate)
        .auth(&alice_drain)
        .send(server.gate);
    assert_eq!(still_there.json()["records"][0]["seq"], hers);
}

/// A request type is a user's own too, and so is the list of them.
#[test]
fn types_are_per_user() {
    let server = start();
    let bob = server.add_user("bob");
    let gate = server.gate.to_string();
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../mecha-manifest/types/meeting.toml"),
    )
    .unwrap();

    let reply = Request::new("PUT", "/v1/types/meeting", &gate)
        .auth(&server.key(Scope::Release))
        .body(manifest)
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);

    // Bob has none, and cannot read hers.
    let bob_key = server.key_for(&bob, Scope::Publish);
    let listed = Request::new("GET", "/v1/types", &gate)
        .auth(&bob_key)
        .send(server.gate);
    assert_eq!(listed.json()["types"].as_array().unwrap().len(), 0);
    assert_eq!(
        Request::new("GET", "/v1/types/meeting", &gate)
            .auth(&bob_key)
            .send(server.gate)
            .status,
        404
    );
}

/// Suspension has to mean the account stops working, not that most of it does.
#[test]
fn a_suspended_user_stops_serving_and_stops_publishing() {
    let server = start();
    let gate = server.gate.to_string();
    let key = server.key(Scope::Publish);
    let key_rel = server.key(Scope::Release);
    Request::new("POST", "/v1/bundles", &gate)
        .auth(&key)
        .body(bundle_archive("brief", 1, ContentClass::Static, "Monday"))
        .send(server.gate);
    Request::new("POST", "/v1/bundles/brief/alias", &gate)
        .auth(&key_rel)
        .body(r#"{"version":1,"visibility":"public"}"#)
        .send(server.gate);
    assert_eq!(server.get(server.artifacts, "/b/brief/v/1/").status, 200);

    server.db.user_status(&server.user.id, "suspended").unwrap();

    // The reader gets what a name that never existed gets.
    assert_eq!(server.get(server.artifacts, "/b/brief/v/1/").status, 404);
    // And a live key belonging to a suspended account is refused — with 403
    // rather than 401, because the token is real and the account is not usable.
    let reply = Request::new("POST", "/v1/bundles", &gate)
        .auth(&key)
        .body(bundle_archive("other", 1, ContentClass::Static, "x"))
        .send(server.gate);
    assert_eq!(reply.status, 403, "{}", reply.body);
    assert!(reply.body.contains("suspended"), "{}", reply.body);

    // Nothing was destroyed by any of it.
    server.db.user_status(&server.user.id, "active").unwrap();
    assert_eq!(server.get(server.artifacts, "/b/brief/v/1/").status, 200);
}

/// A handle is never issued twice, so a URL somebody put in a paper can never
/// come to resolve to somebody else's content.
#[test]
fn a_handle_is_never_issued_twice() {
    let server = start();
    let err = server
        .db
        .user_create("alice", "someone-else@example.org", "2026-08-07T00:00:00Z")
        .unwrap_err()
        .to_string();
    assert!(err.contains("never reused"), "{err}");

    // Not even after the account stops working: suspension is not a release.
    server.db.user_status(&server.user.id, "suspended").unwrap();
    assert!(server
        .db
        .user_create("alice", "someone-else@example.org", "2026-08-07T00:00:00Z")
        .is_err());
}

/// The bytes stay, and nobody can read them. Both halves matter: withholding
/// in response to a report must not destroy the evidence the report is about.
#[test]
fn a_withheld_version_is_served_to_nobody_and_still_on_disk() {
    let server = start();
    let gate = server.gate.to_string();
    let key = server.key(Scope::Publish);
    let key_rel = server.key(Scope::Release);
    Request::new("POST", "/v1/bundles", &gate)
        .auth(&key)
        .body(bundle_archive("brief", 1, ContentClass::Static, "Monday"))
        .send(server.gate);
    Request::new("POST", "/v1/bundles/brief/alias", &gate)
        .auth(&key_rel)
        .body(r#"{"version":1,"visibility":"public"}"#)
        .send(server.gate);
    assert_eq!(server.get(server.artifacts, "/b/brief/v/1/").status, 200);

    server
        .db
        .bundle_withhold(&server.user.id, "brief", 1, Some("reported"), Some("now"))
        .unwrap();

    assert_eq!(server.get(server.artifacts, "/b/brief/v/1/").status, 404);
    let on_disk = server
        .dir
        .path()
        .join("bundles")
        .join(&server.user.id)
        .join("brief/1/index.html");
    assert!(on_disk.is_file(), "the evidence was destroyed");

    // Reversible, because a report that turns out to be wrong should cost
    // nothing.
    server
        .db
        .bundle_withhold(&server.user.id, "brief", 1, None, None)
        .unwrap();
    assert_eq!(server.get(server.artifacts, "/b/brief/v/1/").status, 200);
}

/// A global cap stopped being a cap the moment the disk had more than one
/// tenant on it.
#[test]
fn a_quota_is_per_user_and_refuses_before_the_bytes_land() {
    let server = start();
    let gate = server.gate.to_string();
    let key = server.key(Scope::Publish);
    server.db.user_quota(&server.user.id, 10).unwrap();

    let reply = Request::new("POST", "/v1/bundles", &gate)
        .auth(&key)
        .body(bundle_archive("brief", 1, ContentClass::Static, "Monday"))
        .send(server.gate);
    assert_eq!(reply.status, 507, "{}", reply.body);
    assert!(reply.body.contains("quota"), "{}", reply.body);
    assert_eq!(server.db.bundle_count().unwrap(), 0);
    assert!(!server
        .dir
        .path()
        .join("bundles")
        .join(&server.user.id)
        .join("brief")
        .exists());

    // Bob's quota is his own.
    let bob = server.add_user("bob");
    let reply = Request::new("POST", "/v1/bundles", &gate)
        .auth(&server.key_for(&bob, Scope::Publish))
        .body(bundle_archive("brief", 1, ContentClass::Static, "Monday"))
        .send(server.gate);
    assert_eq!(reply.status, 201, "{}", reply.body);
}

/// A hostname nobody owns serves nothing — including the bare origin, which
/// has no "default user" for the same reason there is no default origin.
#[test]
fn an_unowned_hostname_serves_nothing() {
    let server = start();
    let gate = server.gate.to_string();
    let key = server.key(Scope::Publish);
    let key_rel = server.key(Scope::Release);
    Request::new("POST", "/v1/bundles", &gate)
        .auth(&key)
        .body(bundle_archive("brief", 1, ContentClass::Static, "Monday"))
        .send(server.gate);
    Request::new("POST", "/v1/bundles/brief/alias", &gate)
        .auth(&key_rel)
        .body(r#"{"version":1,"visibility":"public"}"#)
        .send(server.gate);

    // The bare artifact origin.
    assert_eq!(
        Request::new("GET", "/b/brief/v/1/", server.artifacts.to_string())
            .send(server.artifacts)
            .status,
        404
    );
    // A handle that was never issued.
    assert_eq!(
        server
            .get_as(server.artifacts, "mallory", "/b/brief/v/1/")
            .status,
        404
    );
}
