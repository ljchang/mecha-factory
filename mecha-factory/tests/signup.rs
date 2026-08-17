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

/// The open door, end to end: an address nobody vouched for becomes an
/// account, through the same rows and the same claim form an operator-minted
/// invite goes through.
///
/// This is the whole of "anybody may create an account", and it is one test
/// because it is one mechanism — asking mints the invite the operator would
/// have minted, and everything after that is covered by the tests above.
#[test]
fn anybody_may_ask_and_the_ask_waits_for_the_operator() {
    let server = common::start();

    let form = get(&server, "/signup");
    assert_eq!(form.status, 200, "{}", form.head);
    assert!(form.body.contains("Create an account"), "{}", form.body);
    assert!(form.body.contains("reviewed by a person"), "{}", form.body);

    let asked = post(&server, "/signup", "email=casey%40example.org");
    assert_eq!(asked.status, 200, "{}", asked.body);
    assert!(asked.body.contains("Request received"), "{}", asked.body);

    // Nothing mails and nothing mints until the operator decides: the ask
    // is a row, not an invite.
    assert_eq!(server.sent_links(), 0, "an ask must not mail");
    assert!(
        server.db.invites().unwrap().is_empty(),
        "an ask must not mint"
    );
    let pending = server.db.asks_pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].email, "casey@example.org");
}

/// An address that already has an account, and an address that already asked
/// today, get the page a brand-new address gets — the same bytes — and no
/// second mail.
///
/// The form is unauthenticated and anyone may post to it, so a page that
/// varied would be a membership oracle for every address somebody cares to
/// type. Asserted on the body and not just the status, because equal statuses
/// with different words would leak just as well.
#[test]
fn the_ask_page_never_says_whether_an_address_is_known() {
    let server = common::start();

    let fresh = post(&server, "/signup", "email=casey%40example.org");

    // The same address again: already waiting.
    let again = post(&server, "/signup", "email=casey%40example.org");
    assert_eq!(again.status, fresh.status);
    assert_eq!(again.body, fresh.body, "a repeat ask was distinguishable");
    assert_eq!(
        server.db.asks_pending().unwrap().len(),
        1,
        "a repeat ask must fold into the waiting row"
    );

    // An address that already holds an account — the server's own user.
    let existing = format!("email={}", server.user.email.replace('@', "%40"));
    let known = post(&server, "/signup", &existing);
    assert_eq!(known.status, fresh.status);
    assert_eq!(
        known.body, fresh.body,
        "a known address was distinguishable"
    );
    assert_eq!(
        server.db.asks_pending().unwrap().len(),
        1,
        "a known address must not join the queue"
    );
    // And in every case: no mail. Mail is approval's to send.
    assert_eq!(server.sent_links(), 0);
}

/// A spent week queues asks instead of turning people away.
///
/// Under the approval design the budget is enforced at approval, on the
/// panel (`tests/operator.rs`); the ask page has no paused state to show,
/// because an ask no longer spends anything. The failure the old 503 page
/// guarded against — minting a handle whose certificate cannot be issued —
/// cannot happen from here anymore: nothing mints.
#[test]
fn a_spent_week_still_queues_asks() {
    let server = common::start();
    let now = mecha_factory::db::now();
    for n in 0..31 {
        server
            .db
            .user_create(&format!("u{n}"), &format!("u{n}@example.org"), &now)
            .unwrap();
    }
    for n in 0..9 {
        server
            .db
            .invite_create(
                &format!("pending{n}@example.org"),
                "test",
                &hash_token(&mint_token()),
                &now,
                "2027-01-01T00:00:00Z",
            )
            .unwrap();
    }

    let form = get(&server, "/signup");
    assert_eq!(form.status, 200, "{}", form.head);

    let asked = post(&server, "/signup", "email=casey%40example.org");
    assert_eq!(asked.status, 200, "{}", asked.head);
    assert!(asked.body.contains("Request received"), "{}", asked.body);
    assert_eq!(server.sent_links(), 0, "a spent week must mail nothing");
    assert_eq!(server.db.asks_pending().unwrap().len(), 1);
}

/// One connection may ask a few times a day and no more, so a single host
/// cannot spend the week's budget on addresses it owns.
///
/// Told plainly, unlike the address checks above: this is a fact about the
/// asker's own connection, and a person behind a shared address who is never
/// going to get mail deserves to know why.
#[test]
fn one_address_may_only_ask_so_often() {
    let server = common::start();
    for n in 0..3 {
        let reply = post(&server, "/signup", &format!("email=a{n}%40example.org"));
        assert_eq!(reply.status, 200, "ask {n}: {}", reply.head);
    }
    assert_eq!(server.sent_links(), 0, "asks never mail; approval does");

    let refused = post(&server, "/signup", "email=a3%40example.org");
    assert_eq!(refused.status, 429, "{}", refused.head);
    assert!(
        refused.body.contains("Too many requests"),
        "{}",
        refused.body
    );
    assert_eq!(server.sent_links(), 0, "a refused ask must mail nothing");
}

/// A typo is the asker's own business and is told back to them; nothing about
/// it spends the day's asks.
#[test]
fn a_malformed_address_is_a_form_error_and_not_a_spent_ask() {
    let server = common::start();
    let bad = post(&server, "/signup", "email=not-an-address");
    assert_eq!(bad.status, 400, "{}", bad.head);
    assert!(bad.body.contains("email address"), "{}", bad.body);

    // All three real asks are still available afterwards.
    for n in 0..3 {
        let reply = post(&server, "/signup", &format!("email=b{n}%40example.org"));
        assert_eq!(reply.status, 200, "ask {n}: {}", reply.head);
    }
    assert_eq!(server.db.asks_pending().unwrap().len(), 3);
}

/// The ask routes are the gate's, like the claim routes beside them.
#[test]
fn asking_answers_only_on_the_gate() {
    let server = common::start();
    let elsewhere =
        Request::new("GET", "/signup", server.host(server.artifacts)).send(server.artifacts);
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
