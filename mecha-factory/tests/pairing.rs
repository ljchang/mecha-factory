//! Spending a pairing code, against the real server.
//!
//! Two properties carry it. **The assertion is the server's to check**: a
//! redemption names the handle it expects, and a mismatch spends nothing and
//! answers exactly what a code that never existed answers — so no client can
//! skip the confirmation, and a stolen code cannot be probed for whose it is.
//! And **the keys a pairing mints are ordinary keys**: they authenticate the
//! same endpoints a `factory key create` key does, carry the label the
//! machine sent, and revoke one at a time.

mod common;

use common::Request;
use mecha_factory::db::Scope;

fn pair_code(server: &common::Server) -> String {
    mecha_factory::keys::mint_pairing(&server.db, &server.user.id).unwrap()
}

fn redeem(server: &common::Server, code: &str, handle: &str, label: &str) -> common::Reply {
    let body = serde_json::json!({ "code": code, "handle": handle, "label": label });
    Request::new("POST", "/v1/pair", server.gate.to_string())
        .header("Content-Type", "application/json")
        .body(body.to_string().into_bytes())
        .send(server.gate)
}

/// The whole flow: redeem with the right assertion, get two working keys.
#[test]
fn a_code_becomes_two_working_keys() {
    let server = common::start();
    let code = pair_code(&server);

    let reply = redeem(&server, &code, "alice", "test-rig");
    assert_eq!(reply.status, 200, "{}", reply.body);
    let body = reply.json();
    assert_eq!(body["handle"], "alice");

    // The minted keys authenticate as what they claim: publish reads types,
    // drain reads the queue, and neither does the other's job.
    let publish = body["publish_key"].as_str().unwrap();
    let drain = body["drain_key"].as_str().unwrap();
    let types = Request::new("GET", "/v1/types", server.gate.to_string())
        .auth(publish)
        .send(server.gate);
    assert_eq!(types.status, 200, "{}", types.body);
    let queue = Request::new("GET", "/v1/queue", server.gate.to_string())
        .auth(drain)
        .send(server.gate);
    assert_eq!(queue.status, 200, "{}", queue.body);
    let crossed = Request::new("GET", "/v1/queue", server.gate.to_string())
        .auth(publish)
        .send(server.gate);
    assert_eq!(crossed.status, 403, "{}", crossed.body);

    // The ledger traces the pairing to its keys, and the label rode along.
    let keys = server.db.keys().unwrap();
    let publish_id = body["publish_key_id"].as_str().unwrap();
    let row = keys.iter().find(|k| k.id == publish_id).unwrap();
    assert_eq!(row.label, "test-rig");
    assert_eq!(row.scope, Scope::Publish);
}

/// A wrong assertion is indistinguishable from a code that never existed,
/// and it spends nothing: the right assertion still works afterwards.
#[test]
fn a_wrong_assertion_spends_nothing_and_reveals_nothing() {
    let server = common::start();
    let code = pair_code(&server);

    let wrong = redeem(&server, &code, "mallory", "laptop");
    let never = redeem(&server, "nosuchcode", "alice", "laptop");
    assert_eq!(wrong.status, 404, "{}", wrong.body);
    assert_eq!(
        wrong.body, never.body,
        "a mismatch must not be tellable apart"
    );

    // No keys were minted for the refused attempt…
    assert!(server.db.keys().unwrap().is_empty());
    // …and the code survived to be redeemed properly.
    assert_eq!(redeem(&server, &code, "alice", "laptop").status, 200);
}

/// Spent is spent: a second redemption is refused like any dead code, and
/// mints nothing.
#[test]
fn a_code_is_spent_exactly_once() {
    let server = common::start();
    let code = pair_code(&server);
    assert_eq!(redeem(&server, &code, "alice", "one").status, 200);
    assert_eq!(redeem(&server, &code, "alice", "two").status, 404);
    assert_eq!(server.db.keys().unwrap().len(), 2, "one pairing, two keys");
}

/// Expiry and suspension both refuse — the code outliving its minutes, and
/// the account being suspended between mint and redeem.
#[test]
fn expired_codes_and_suspended_accounts_are_refused() {
    let server = common::start();

    let stale = mecha_factory::intake::mint_token();
    server
        .db
        .pairing_create(
            &server.user.id,
            &mecha_factory::intake::hash_token(&stale),
            "2026-08-07T00:00:00Z",
            "2026-08-07T00:00:01Z",
        )
        .unwrap();
    assert_eq!(redeem(&server, &stale, "alice", "x").status, 404);

    let code = pair_code(&server);
    server.db.user_status(&server.user.id, "suspended").unwrap();
    assert_eq!(redeem(&server, &code, "alice", "x").status, 404);

    // Sweep removes the expired one and keeps the unredeemed-but-live one.
    let swept = server
        .db
        .expire_pairings(&mecha_factory::db::now())
        .unwrap();
    assert_eq!(swept, 1);
}

/// Every pairing mints its own keys: two machines, two pairs, revocable one
/// at a time — which is the multiple-agents story in one test.
#[test]
fn each_machine_pairs_separately_and_dies_separately() {
    let server = common::start();
    let laptop = redeem(&server, &pair_code(&server), "alice", "laptop");
    let dgx = redeem(&server, &pair_code(&server), "alice", "dgx");
    assert_eq!(laptop.status, 200);
    assert_eq!(dgx.status, 200);

    // Revoking the laptop's publish key leaves the DGX publishing.
    let laptop_key = laptop.json()["publish_key_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(server
        .db
        .key_revoke(&laptop_key, &mecha_factory::db::now())
        .unwrap());
    let dead = Request::new("GET", "/v1/types", server.gate.to_string())
        .auth(laptop.json()["publish_key"].as_str().unwrap())
        .send(server.gate);
    assert_eq!(dead.status, 401, "{}", dead.body);
    let alive = Request::new("GET", "/v1/types", server.gate.to_string())
        .auth(dgx.json()["publish_key"].as_str().unwrap())
        .send(server.gate);
    assert_eq!(alive.status, 200, "{}", alive.body);
}
