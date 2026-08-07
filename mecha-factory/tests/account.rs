//! The tenant surface, against the real server.
//!
//! The property that carries the suite: **a session is a release credential
//! and nothing more** — it moves aliases and kills keys for exactly one
//! account, and every way of getting one (the only way: a link sent to the
//! account's address) is single-use, budgeted, and oracle-free.

mod common;

use common::{Reply, Request, Server};
use mecha_factory::db::Scope;

fn post(server: &Server, target: &str, body: &str, cookie: Option<&str>) -> Reply {
    let mut request = Request::new("POST", target, server.gate.to_string())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body.as_bytes().to_vec());
    if let Some(cookie) = cookie {
        request = request.header("Cookie", &format!("__Host-factory-session={cookie}"));
    }
    request.send(server.gate)
}

fn get(server: &Server, target: &str, cookie: Option<&str>) -> Reply {
    let mut request = Request::new("GET", target, server.gate.to_string());
    if let Some(cookie) = cookie {
        request = request.header("Cookie", &format!("__Host-factory-session={cookie}"));
    }
    request.send(server.gate)
}

/// Sign in as the fixture user and hand back the session token.
fn signed_in(server: &Server) -> String {
    let asked = post(server, "/account/signin", "email=alice%40example.org", None);
    assert_eq!(asked.status, 200, "{}", asked.body);
    let link = server.verification_token(); // the last link the mailer saw
    let finished = get(server, &format!("/account/s/{link}"), None);
    assert_eq!(finished.status, 303, "{}", finished.head);
    let cookie = finished.header("set-cookie").expect("a session cookie");
    // The attributes are the security, so they are asserted, not assumed.
    for needed in ["__Host-", "HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
        assert!(cookie.contains(needed), "cookie lacks {needed}: {cookie}");
    }
    cookie
        .split_once('=')
        .unwrap()
        .1
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

/// The CSRF value the page embeds, scraped from the overview.
fn csrf_of(server: &Server, session: &str) -> String {
    let page = get(server, "/account", Some(session));
    assert_eq!(page.status, 200, "{}", page.body);
    let marker = "name=\"csrf\" value=\"";
    let start = page.body.find(marker).expect("a csrf field") + marker.len();
    page.body[start..].split('"').next().unwrap().to_string()
}

/// The whole way in: ask, click, arrive — and the link works exactly once.
#[test]
fn a_link_signs_in_once_and_the_cookie_is_armoured() {
    let server = common::start();
    let session = signed_in(&server);

    let page = get(&server, "/account", Some(&session));
    assert_eq!(page.status, 200);
    assert!(page.body.contains("alice"), "{}", page.body);
    assert!(page.body.contains("Machines"), "{}", page.body);

    // The spent link is any dead token.
    let link = server.verification_token();
    let again = get(&server, &format!("/account/s/{link}"), None);
    assert_eq!(again.status, 404, "{}", again.head);

    // No cookie, no page — the sign-in form instead.
    let out = get(&server, "/account", None);
    assert!(out.body.contains("Sign in"), "{}", out.body);
}

/// One answer whoever asked: an address with no account gets the same bytes,
/// and sends nothing.
#[test]
fn the_signin_form_is_not_a_directory() {
    let server = common::start();
    let known = post(
        &server,
        "/account/signin",
        "email=alice%40example.org",
        None,
    );
    let sent_before = server.sent_links();
    let unknown = post(
        &server,
        "/account/signin",
        "email=nobody%40example.org",
        None,
    );
    assert_eq!(known.status, unknown.status);
    assert_eq!(known.body, unknown.body);
    assert_eq!(
        server.sent_links(),
        sent_before,
        "an unknown address must send nothing"
    );
}

/// Release authority, live: the page's POST moves the alias the same way the
/// release key does — and a wrong CSRF token moves nothing.
#[test]
fn a_session_releases_and_a_stale_form_does_not() {
    let server = common::start();
    let session = signed_in(&server);

    // A version to release, planted the way a machine's push plants one.
    server
        .db
        .bundle_insert(&mecha_factory::db::BundleRow {
            user_id: server.user.id.clone(),
            id: "brief".into(),
            version: 1,
            digest: "d".into(),
            class: mecha_manifest::ContentClass::Static,
            title: "A briefing".into(),
            description: None,
            template: "report".into(),
            published_at: None,
            received_at: mecha_factory::db::now(),
            withheld_at: None,
            withheld_reason: None,
        })
        .unwrap();

    // Wrong CSRF: refused, nothing moves.
    let forged = post(
        &server,
        "/account/release",
        "csrf=wrong&id=brief&version=1&visibility=public",
        Some(&session),
    );
    assert_eq!(forged.status, 403, "{}", forged.body);
    assert!(server.db.alias(&server.user.id, "brief").unwrap().is_none());

    // The real form releases.
    let csrf = csrf_of(&server, &session);
    let released = post(
        &server,
        "/account/release",
        &format!("csrf={csrf}&id=brief&version=1&visibility=public"),
        Some(&session),
    );
    assert_eq!(released.status, 200, "{}", released.body);
    let alias = server.db.alias(&server.user.id, "brief").unwrap().unwrap();
    assert_eq!(alias.version, Some(1));
    assert_eq!(alias.visibility, mecha_manifest::Visibility::Public);

    // And back to private, which is the takedown a person reaches for.
    let hidden = post(
        &server,
        "/account/release",
        &format!("csrf={csrf}&id=brief&visibility=private"),
        Some(&session),
    );
    assert_eq!(hidden.status, 200);
    let alias = server.db.alias(&server.user.id, "brief").unwrap().unwrap();
    assert_eq!(alias.visibility, mecha_manifest::Visibility::Private);
}

/// The machine list is live: a used key shows life, revoking from the page
/// kills it, and somebody else's key id kills nothing.
#[test]
fn machines_are_listed_used_and_revocable_from_the_page() {
    let server = common::start();
    let session = signed_in(&server);

    let minted =
        mecha_factory::keys::mint(&server.db, &server.user.id, Scope::Publish, "laptop").unwrap();
    // Using the key stamps it.
    let used = Request::new("GET", "/v1/types", server.gate.to_string())
        .auth(&minted.token)
        .send(server.gate);
    assert_eq!(used.status, 200);
    let page = get(&server, "/account", Some(&session));
    assert!(page.body.contains("laptop"), "{}", page.body);
    let keys = server.db.keys_for_user(&server.user.id).unwrap();
    assert!(keys.iter().any(|k| k.last_used_at.is_some()));

    // Bob's key is not alice's to kill.
    let bob = server.add_user("bob");
    let bobs = mecha_factory::keys::mint(&server.db, &bob.id, Scope::Publish, "bobs").unwrap();
    let csrf = csrf_of(&server, &session);
    post(
        &server,
        "/account/revoke",
        &format!("csrf={csrf}&key={}", bobs.row.id),
        Some(&session),
    );
    assert!(server
        .db
        .key_by_id(&bobs.row.id)
        .unwrap()
        .unwrap()
        .revoked_at
        .is_none());

    // Alice's is.
    post(
        &server,
        "/account/revoke",
        &format!("csrf={csrf}&key={}", minted.row.id),
        Some(&session),
    );
    let dead = Request::new("GET", "/v1/types", server.gate.to_string())
        .auth(&minted.token)
        .send(server.gate);
    assert_eq!(dead.status, 401, "{}", dead.body);
}

/// Sign-out revokes the row, not just the cookie — the same token presented
/// again is nobody.
#[test]
fn signing_out_ends_the_session_server_side() {
    let server = common::start();
    let session = signed_in(&server);
    let csrf = csrf_of(&server, &session);
    let out = post(
        &server,
        "/account/signout",
        &format!("csrf={csrf}"),
        Some(&session),
    );
    assert_eq!(out.status, 303);
    let after = get(&server, "/account", Some(&session));
    assert!(after.body.contains("Sign in"), "{}", after.body);
}

/// Suspension ends the signed-in page with everything else, mid-session.
#[test]
fn a_suspended_account_loses_its_session() {
    let server = common::start();
    let session = signed_in(&server);
    server.db.user_status(&server.user.id, "suspended").unwrap();
    let page = get(&server, "/account", Some(&session));
    assert!(page.body.contains("Sign in"), "{}", page.body);
}

/// The sign-in budget holds: the sixth ask in a day sends nothing, and the
/// page says exactly what it always says.
#[test]
fn signin_links_are_budgeted_per_day() {
    let server = common::start();
    for _ in 0..mecha_factory::intake::SIGNIN_LINKS_PER_DAY {
        post(
            &server,
            "/account/signin",
            "email=alice%40example.org",
            None,
        );
    }
    let sent = server.sent_links();
    let over = post(
        &server,
        "/account/signin",
        "email=alice%40example.org",
        None,
    );
    assert_eq!(over.status, 200);
    assert!(over.body.contains("Check your email"));
    assert_eq!(server.sent_links(), sent, "the budget held");
}

/// A key retires itself: disconnect revokes exactly the presented key.
#[test]
fn a_key_can_disconnect_itself_and_only_itself() {
    let server = common::start();
    let one =
        mecha_factory::keys::mint(&server.db, &server.user.id, Scope::Publish, "one").unwrap();
    let two = mecha_factory::keys::mint(&server.db, &server.user.id, Scope::Drain, "two").unwrap();

    let gone = Request::new("POST", "/v1/disconnect", server.gate.to_string())
        .auth(&one.token)
        .body(Vec::new())
        .send(server.gate);
    assert_eq!(gone.status, 200, "{}", gone.body);
    assert_eq!(gone.json()["revoked"], one.row.id.as_str());

    // The revoked key is nobody now — even to disconnect.
    let again = Request::new("POST", "/v1/disconnect", server.gate.to_string())
        .auth(&one.token)
        .body(Vec::new())
        .send(server.gate);
    assert_eq!(again.status, 401, "{}", again.body);

    // The other key is untouched.
    assert!(server
        .db
        .key_by_id(&two.row.id)
        .unwrap()
        .unwrap()
        .revoked_at
        .is_none());
}

/// The chrome: the mark on gate pages, the account dropdown only for a
/// session, and none of it on the pages whose byte-identity is a security
/// property — a header asset there would be a new oracle.
#[test]
fn the_header_knows_who_it_is_for() {
    let server = common::start();

    // Signed out: public chrome — the mark, no dropdown.
    let signin = server.get(server.gate, "/account");
    assert!(signin.body.contains("header class=\"site\""), "{}", signin.body);
    assert!(!signin.body.contains("account-menu"));

    // Signed in: the dropdown, holding the identity and the sign-out.
    let cookie = signed_in(&server);
    let page = get(&server, "/account", Some(&cookie));
    assert!(page.body.contains("account-menu"), "{}", page.body);
    assert!(page.body.contains("/account/signout"));
    assert!(page.body.contains("alice"));

    // A 404 that must not distinguish handles carries no chrome at all.
    let missing = server.get(server.gate, "/f/nobody/nothing");
    assert!(!missing.body.contains("header class=\"site\""), "{}", missing.body);
    assert!(!missing.body.contains("form.css"));
}
