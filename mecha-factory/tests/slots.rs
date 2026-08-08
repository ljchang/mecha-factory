//! The slot push: home replacing an instrument's availability cache, and
//! everything the endpoint refuses — against a real server, with real keys.

mod common;

use common::{start, Request};
use mecha_factory::db::Scope;

fn push_body(slots: &[(&str, &str, u32)]) -> String {
    let slots: Vec<serde_json::Value> = slots
        .iter()
        .map(|(start, end, minutes)| {
            serde_json::json!({"start": start, "end": end, "duration_minutes": minutes})
        })
        .collect();
    serde_json::json!({
        "generated_at": "2026-08-08T12:00:00Z",
        "horizon_days": 60,
        "slots": slots,
    })
    .to_string()
}

#[test]
fn a_slots_key_replaces_the_cache_wholesale() {
    let server = start();
    let gate = server.gate.to_string();
    let key = server.key(Scope::Slots);

    let reply = Request::new("PUT", "/v1/instruments/book/slots", &gate)
        .auth(&key)
        .body(push_body(&[
            ("2026-08-11T17:00:00Z", "2026-08-11T17:30:00Z", 30),
            ("2026-08-11T17:00:00Z", "2026-08-11T18:00:00Z", 60),
        ]))
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["stored"], 2);

    // A second push replaces — the cache is one generation, never a merge of
    // two: slots home withdrew must not survive beside slots it sent.
    let reply = Request::new("PUT", "/v1/instruments/book/slots", &gate)
        .auth(&key)
        .body(push_body(&[(
            "2026-08-13T17:00:00Z",
            "2026-08-13T17:30:00Z",
            30,
        )]))
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);

    let row = server
        .db
        .slots_get(&server.user.id, "book")
        .unwrap()
        .expect("a cache row");
    let stored: Vec<serde_json::Value> = serde_json::from_str(&row.slots).unwrap();
    assert_eq!(stored.len(), 1, "the first generation is gone");
    assert_eq!(stored[0]["start"], "2026-08-13T17:00:00Z");
    assert_eq!(row.generated_at, "2026-08-08T12:00:00Z");
}

#[test]
fn every_other_scope_is_refused_and_so_is_no_key() {
    let server = start();
    let gate = server.gate.to_string();
    let body = push_body(&[("2026-08-11T17:00:00Z", "2026-08-11T17:30:00Z", 30)]);

    for scope in [Scope::Publish, Scope::Release, Scope::Drain] {
        let reply = Request::new("PUT", "/v1/instruments/book/slots", &gate)
            .auth(&server.key(scope))
            .body(body.clone())
            .send(server.gate);
        assert_eq!(
            reply.status,
            403,
            "{} must not push slots: {}",
            scope.as_str(),
            reply.body
        );
    }
    let reply = Request::new("PUT", "/v1/instruments/book/slots", &gate)
        .body(body)
        .send(server.gate);
    assert_eq!(reply.status, 401);
    assert!(
        server
            .db
            .slots_get(&server.user.id, "book")
            .unwrap()
            .is_none(),
        "nothing may have landed"
    );
}

#[test]
fn a_push_that_disagrees_with_itself_is_refused() {
    let server = start();
    let gate = server.gate.to_string();
    let key = server.key(Scope::Slots);
    let put = |body: String| {
        Request::new("PUT", "/v1/instruments/book/slots", &gate)
            .auth(&key)
            .body(body)
            .send(server.gate)
    };

    // A duration that disagrees with its own span.
    let reply = put(push_body(&[(
        "2026-08-11T17:00:00Z",
        "2026-08-11T18:00:00Z",
        30,
    )]));
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(reply.body.contains("disagrees"), "{}", reply.body);

    // An end before its start.
    let reply = put(push_body(&[(
        "2026-08-11T18:00:00Z",
        "2026-08-11T17:00:00Z",
        60,
    )]));
    assert_eq!(reply.status, 400);

    // A stamp that is not a stamp.
    let reply = put(push_body(&[("tuesdayish", "2026-08-11T17:30:00Z", 30)]));
    assert_eq!(reply.status, 400);

    // A field this code never looked at must not ride into the ledger.
    let reply = put(serde_json::json!({
        "generated_at": "2026-08-08T12:00:00Z",
        "horizon_days": 60,
        "slots": [],
        "note": "<script>alert(1)</script>",
    })
    .to_string());
    assert_eq!(reply.status, 400, "{}", reply.body);

    // An id that is not an id.
    let reply = Request::new("PUT", "/v1/instruments/Bo%20ok/slots", &gate)
        .auth(&key)
        .body(push_body(&[]))
        .send(server.gate);
    assert_eq!(reply.status, 400);

    assert!(
        server
            .db
            .slots_get(&server.user.id, "book")
            .unwrap()
            .is_none(),
        "no refused push may leave a row"
    );
}

/// Two tenants may both run an instrument called `book`; each key writes its
/// own row and neither can touch the other's.
#[test]
fn slot_caches_are_tenant_scoped() {
    let server = start();
    let gate = server.gate.to_string();
    let bob = server.add_user("bob");
    let bob_key = server.key_for(&bob, Scope::Slots);

    let reply = Request::new("PUT", "/v1/instruments/book/slots", &gate)
        .auth(&bob_key)
        .body(push_body(&[(
            "2026-08-11T17:00:00Z",
            "2026-08-11T17:30:00Z",
            30,
        )]))
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);

    assert!(server.db.slots_get(&bob.id, "book").unwrap().is_some());
    assert!(
        server
            .db
            .slots_get(&server.user.id, "book")
            .unwrap()
            .is_none(),
        "bob's push may not appear under alice"
    );
}

#[test]
fn the_endpoint_exists_only_on_the_gate() {
    let server = start();
    let key = server.key(Scope::Slots);
    let reply = Request::new(
        "PUT",
        "/v1/instruments/book/slots",
        server.host(server.artifacts),
    )
    .auth(&key)
    .body(push_body(&[]))
    .send(server.artifacts);
    assert_eq!(reply.status, 404, "{}", reply.body);
}
