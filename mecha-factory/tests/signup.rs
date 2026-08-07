//! Claiming a handle from an invite, against the real server.
//!
//! The property that carries the suite: **the signup path and the CLI path
//! are one mechanism.** A user claimed through the form is a row
//! `user_create` could have written — same tables, same handle rules, same
//! never-reissued guarantee — so everything already true of users stays true
//! of signed-up ones without new tests.
//!
//! And its counterpart: **a dead invite is one page.** Claimed, revoked,
//! expired and never-existed answer with the same bytes, because which of
//! the four it was is not the visitor's business.

mod common;

use common::Request;
use mecha_factory::db::Claim;
use mecha_factory::intake::{hash_token, mint_token};

/// An invite minted the way the CLI mints one, returning the token the link
/// would carry.
fn invite(server: &common::Server, email: &str) -> String {
    invite_expiring(server, email, "2027-01-01T00:00:00Z")
}

fn invite_expiring(server: &common::Server, email: &str, expires: &str) -> String {
    let token = mint_token();
    server
        .db
        .invite_create(
            email,
            "test",
            &hash_token(&token),
            "2026-08-07T00:00:00Z",
            expires,
        )
        .unwrap();
    token
}

fn get(server: &common::Server, target: &str) -> common::Reply {
    Request::new("GET", target, server.gate.to_string()).send(server.gate)
}

fn post(server: &common::Server, target: &str, body: &str) -> common::Reply {
    Request::new("POST", target, server.gate.to_string())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body.as_bytes().to_vec())
        .send(server.gate)
}

/// The whole flow: a live invite renders the form, the claim creates the
/// user through the same path the CLI uses, and the invite is spent by it.
#[test]
fn an_invite_becomes_an_account_and_is_spent_by_it() {
    let server = common::start();
    let token = invite(&server, "casey@example.org");

    let form = get(&server, &format!("/signup/{token}"));
    assert_eq!(form.status, 200, "{}", form.head);
    assert!(form.body.contains("Claim your handle"), "{}", form.body);
    assert!(form.body.contains("permanent"), "{}", form.body);

    let done = post(&server, &format!("/signup/{token}"), "handle=casey");
    assert_eq!(done.status, 200, "{}", done.body);
    assert!(done.body.contains("You are"), "{}", done.body);
    assert!(done.body.contains("casey."), "{}", done.body);

    // The row is an ordinary user: the address the invite went to, active,
    // and holding the handle in the never-reissue ledger.
    let user = server.db.user_by_handle("casey").unwrap().unwrap();
    assert_eq!(user.email, "casey@example.org");
    assert!(user.active());

    // Spent: the same link now gets the dead-invite page…
    let again = get(&server, &format!("/signup/{token}"));
    assert_eq!(again.status, 404, "{}", again.head);
    // …and the ledger says who spent it.
    let now = mecha_factory::db::now();
    let rows = server.db.invites().unwrap();
    let row = rows
        .iter()
        .find(|r| r.email == "casey@example.org")
        .unwrap();
    assert_eq!(row.status(&now), "claimed");
    assert_eq!(row.claimed_by.as_deref(), Some(user.id.as_str()));
}

/// A rejected handle is the visitor's form back with the error on it — and
/// the invite still good, because "not that name" is not "not you".
#[test]
fn a_rejected_handle_keeps_the_invite_claimable() {
    let server = common::start();
    let token = invite(&server, "casey@example.org");
    let target = format!("/signup/{token}");

    // Taken: the fixture's user already holds `alice`. One bit of detail,
    // never whose.
    let taken = post(&server, &target, "handle=alice");
    assert_eq!(taken.status, 400, "{}", taken.body);
    assert!(taken.body.contains("not available"), "{}", taken.body);
    assert!(!taken.body.contains("belongs to"), "{}", taken.body);

    // Illegal shape: the rule, and what they typed, kept for editing.
    let illegal = post(&server, &target, "handle=Casey_C");
    assert_eq!(illegal.status, 400, "{}", illegal.body);
    assert!(illegal.body.contains("casey_c"), "{}", illegal.body);

    // Reserved names are refused exactly as the CLI refuses them.
    let reserved = post(&server, &target, "handle=admin");
    assert_eq!(reserved.status, 400, "{}", reserved.body);

    // Three refusals later the invite still works.
    let done = post(&server, &target, "handle=casey");
    assert_eq!(done.status, 200, "{}", done.body);
}

/// Uppercase is the browser's opinion, not the person's choice: `Casey` is
/// claimed as `casey` rather than refused for a rule they cannot see.
#[test]
fn a_capitalised_handle_is_claimed_lowercase() {
    let server = common::start();
    let token = invite(&server, "casey@example.org");
    let done = post(&server, &format!("/signup/{token}"), "handle=+Casey+");
    assert_eq!(done.status, 200, "{}", done.body);
    assert!(server.db.user_by_handle("casey").unwrap().is_some());
}

/// One page, byte for byte, for every kind of dead invite. Any difference —
/// wording, status, even a stylesheet reference — is an oracle for what
/// became of somebody else's link.
#[test]
fn every_dead_invite_is_the_same_page() {
    let server = common::start();

    let spent = invite(&server, "spent@example.org");
    assert_eq!(
        post(&server, &format!("/signup/{spent}"), "handle=spent").status,
        200
    );

    let revoked = invite(&server, "revoked@example.org");
    let now = mecha_factory::db::now();
    let id = server
        .db
        .invites()
        .unwrap()
        .into_iter()
        .find(|r| r.email == "revoked@example.org")
        .unwrap()
        .id;
    assert!(server.db.invite_revoke(&id, &now).unwrap());

    let expired = invite_expiring(&server, "late@example.org", "2026-08-07T00:00:01Z");

    let pages: Vec<common::Reply> = [spent, revoked, expired, mint_token()]
        .iter()
        .map(|token| get(&server, &format!("/signup/{token}")))
        .collect();
    for page in &pages {
        assert_eq!(page.status, 404, "{}", page.head);
        assert_eq!(page.body, pages[0].body);
    }

    // The POST side answers the same way — a dead invite must not get a form
    // response even when the handle it posted was illegal.
    let post_dead = post(&server, &format!("/signup/{}", mint_token()), "handle=??");
    assert_eq!(post_dead.status, 404);
    assert_eq!(post_dead.body, pages[0].body);
}

/// Two clicks race to one account: the second claim of the same invite gets
/// the dead-invite page even when it asked for a different handle.
#[test]
fn an_invite_is_spent_exactly_once() {
    let server = common::start();
    let token = invite(&server, "casey@example.org");
    assert_eq!(
        post(&server, &format!("/signup/{token}"), "handle=casey").status,
        200
    );
    let second = post(&server, &format!("/signup/{token}"), "handle=other");
    assert_eq!(second.status, 404, "{}", second.body);
    assert!(server.db.user_by_handle("other").unwrap().is_none());
}

/// The signup routes are the gate's. On an artifact origin there is no form,
/// and the answer says nothing about whether the token was real.
#[test]
fn signup_answers_only_on_the_gate() {
    let server = common::start();
    let token = invite(&server, "casey@example.org");
    let elsewhere = Request::new(
        "GET",
        &format!("/signup/{token}"),
        server.host(server.artifacts),
    )
    .send(server.artifacts);
    assert_eq!(elsewhere.status, 404, "{}", elsewhere.head);
}

/// The store-level race arm the HTTP tests cannot reach: a claim whose
/// handle check and insert must be one transaction, and a dead invite told
/// apart from a taken handle without matching on error text.
#[test]
fn the_claim_outcomes_are_typed_not_worded() {
    let server = common::start();
    let token = invite(&server, "casey@example.org");
    let hash = hash_token(&token);
    let now = mecha_factory::db::now();

    match server.db.invite_claim(&hash, "alice", &now).unwrap() {
        Claim::HandleTaken => {}
        other => panic!("a taken handle answered {other:?}"),
    }
    match server.db.invite_claim(&hash, "casey", &now).unwrap() {
        Claim::Created(user) => assert_eq!(user.handle, "casey"),
        other => panic!("a live claim answered {other:?}"),
    }
    match server.db.invite_claim(&hash, "casey2", &now).unwrap() {
        Claim::InviteGone => {}
        other => panic!("a spent invite answered {other:?}"),
    }
}
