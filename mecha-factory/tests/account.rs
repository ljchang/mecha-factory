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
                                            // The person's path: interstitial first (which spends nothing — the
                                            // mail-scanner fix), then the button's POST redeems.
    let interstitial = get(server, &format!("/account/s/{link}"), None);
    assert_eq!(interstitial.status, 200, "{}", interstitial.body);
    let finished = post(server, &format!("/account/s/{link}"), "", None);
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
    let again = post(&server, &format!("/account/s/{link}"), "", None);
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

/// The page links its own bundles at the viewer, and a notebook is why that
/// is a fix rather than a preference.
///
/// These links were built with `Role::Artifacts` unconditionally, because
/// `BundleSummary` carries no class — so a compute bundle was linked at an
/// origin it does not live on and rescued by a redirect. A viewer URL is
/// class-independent, and it is also what a person clicking their own row
/// wants: it carries the version menu and the release controls, and for a
/// private bundle the artifact origin serves them nothing at all.
#[test]
fn the_account_page_links_bundles_at_the_viewer_whatever_their_class() {
    let server = common::start();
    let session = signed_in(&server);
    for (id, class) in [
        ("brief", mecha_manifest::ContentClass::Static),
        ("notes", mecha_manifest::ContentClass::Compute),
    ] {
        server
            .db
            .bundle_insert(&mecha_factory::db::BundleRow {
                user_id: server.user.id.clone(),
                id: id.into(),
                version: 1,
                digest: format!("d-{id}"),
                class,
                title: id.into(),
                description: None,
                template: "report".into(),
                published_at: None,
                received_at: mecha_factory::db::now(),
                withheld_at: None,
                withheld_reason: None,
            })
            .unwrap();
    }

    let page = get(&server, "/account", Some(&session));
    assert_eq!(page.status, 200, "{}", page.body);
    let gate = server.gate;
    for id in ["brief", "notes"] {
        assert!(
            page.body
                .contains(&format!("href=\"http://{gate}/view/alice/{id}\"")),
            "{id} is not linked at the viewer: {}",
            page.body
        );
        assert!(
            page.body
                .contains(&format!("href=\"http://{gate}/view/alice/{id}/1\"")),
            "{id}'s version is not linked at the viewer: {}",
            page.body
        );
    }
    // And the wrong-origin link the page used to emit for a notebook is gone,
    // rather than merely joined by a right one.
    assert!(
        !page.body.contains(&format!("alice.{}", server.artifacts)),
        "an artifact-origin link survived: {}",
        page.body
    );
}

/// Moving the account page's links to the viewer took away the last surface
/// offering a **per-version** artifact URL — the immutable one a citation
/// names. The viewer carries it now, and only where it serves.
///
/// Only where it serves is the load-bearing half: a private bundle's version
/// URL answers 404 to its owner too, because the gate mints a capability
/// instead. Linking it while private would be the same "here is a URL that
/// opens" mistake this whole branch removes.
#[test]
fn the_viewer_offers_this_versions_bytes_once_the_bundle_is_public() {
    let server = common::start();
    let session = signed_in(&server);
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

    let bytes = format!("http://alice.{}/b/brief/v/1/", server.artifacts);

    // Private: no bytes link, because that URL serves nobody — not even here.
    let private = get(&server, "/view/alice/brief/1", Some(&session));
    assert_eq!(private.status, 200, "{}", private.body);
    assert!(
        !private.body.contains(&bytes),
        "a private version URL was offered as a link: {}",
        private.body
    );

    // Released: it is the citable URL, and the page says so.
    server
        .db
        .alias_set(
            &server.user.id,
            "brief",
            Some(1),
            mecha_manifest::Visibility::Public,
            &mecha_factory::db::now(),
        )
        .unwrap();
    let public = get(&server, "/view/alice/brief/1", Some(&session));
    assert_eq!(public.status, 200, "{}", public.body);
    assert!(
        public.body.contains(&bytes),
        "the immutable version URL is not reachable from anywhere: {}",
        public.body
    );
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

/// A suspended account's link is a dead link: the redeem demands the user
/// active, so it never spends — no session is minted, and because nothing
/// was burned, the same link works again if the account is restored within
/// its window.
#[test]
fn a_suspended_accounts_link_never_spends() {
    let server = common::start();
    post(
        &server,
        "/account/signin",
        "email=alice%40example.org",
        None,
    );
    let link = server.verification_token();
    server.db.user_status(&server.user.id, "suspended").unwrap();

    let refused = post(&server, &format!("/account/s/{link}"), "", None);
    assert_eq!(refused.status, 404, "{}", refused.head);
    assert!(refused.header("set-cookie").is_none());

    server.db.user_status(&server.user.id, "active").unwrap();
    let finished = post(&server, &format!("/account/s/{link}"), "", None);
    assert_eq!(finished.status, 303, "{}", finished.head);
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

    // Signed out: the same corner holds a Sign in dropdown with the email
    // form — and nothing only a session gets.
    let signin = server.get(server.gate, "/account");
    assert!(
        signin.body.contains("header class=\"site\""),
        "{}",
        signin.body
    );
    assert!(
        signin.body.contains("<summary>Sign in</summary>"),
        "{}",
        signin.body
    );
    assert!(signin.body.contains("action=\"/account/signin\""));
    assert!(!signin.body.contains("/account/signout"));

    // Signed in: the dropdown, holding the identity and the sign-out.
    let cookie = signed_in(&server);
    let page = get(&server, "/account", Some(&cookie));
    assert!(page.body.contains("account-menu"), "{}", page.body);
    assert!(page.body.contains("/account/signout"));
    assert!(
        page.body.contains("/account#artifacts"),
        "dropdown links are absolute"
    );
    // The artifacts table carries the full controls once bundles exist; on an
    // empty account it at least says what would land here.
    assert!(page.body.contains("Nothing published yet") || page.body.contains("Take down"));
    assert!(
        page.body.contains("menu.js"),
        "the dropdown gets its close script"
    );
    assert!(page.body.contains("alice"));

    // The script the page references is served, as JavaScript.
    let script = server.get(server.gate, "/account/a/menu.js");
    assert_eq!(script.status, 200);
    assert!(script.body.contains("account-menu"), "{}", script.body);

    // A 404 that must not distinguish handles carries no chrome at all.
    let missing = server.get(server.gate, "/f/nobody/nothing");
    assert!(
        !missing.body.contains("header class=\"site\""),
        "{}",
        missing.body
    );
    assert!(!missing.body.contains("form.css"));
}

/// The gate viewer wears the session: owner sees the Manage menu and the
/// account dropdown; its controls post the same release endpoint and come
/// back to the viewer; a return address that is not a viewer path is
/// ignored rather than followed.
#[test]
fn the_viewer_knows_its_owner_and_its_controls_return_there() {
    let server = common::start();
    let session = signed_in(&server);
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
    server
        .db
        .alias_set(
            &server.user.id,
            "brief",
            Some(1),
            mecha_manifest::Visibility::Public,
            "t",
        )
        .unwrap();

    // Signed in, on your own bundle: handle in the corner, Manage present.
    let page = get(&server, "/view/alice/brief/1", Some(&session));
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body.contains("<summary>alice</summary>"),
        "{}",
        page.body
    );
    assert!(page.body.contains("<summary>Manage</summary>"));
    assert!(page.body.contains("Take down"));
    assert!(page.body.contains("name=\"return\""));

    // Anonymous on the same page: sign-in corner, no controls at all.
    let anon = get(&server, "/view/alice/brief/1", None);
    assert_eq!(anon.status, 200);
    assert!(anon.body.contains("<summary>Sign in</summary>"));
    assert!(!anon.body.contains("Manage"));

    // A control posts, the alias moves, and you land back on the viewer.
    let csrf = csrf_of(&server, &session);
    let back = post(
        &server,
        "/account/release",
        &format!("csrf={csrf}&id=brief&visibility=private&version=1&return=/view/alice/brief/1"),
        Some(&session),
    );
    assert_eq!(back.status, 303, "{}", back.body);
    assert_eq!(back.header("location").unwrap(), "/view/alice/brief/1");
    assert_eq!(
        server
            .db
            .alias(&server.user.id, "brief")
            .unwrap()
            .unwrap()
            .visibility,
        mecha_manifest::Visibility::Private
    );

    // Private now: the owner still gets the viewer; a visitor with no
    // session gets the reader sign-in gate — the page every private or
    // absent viewer URL answers with, so it confirms nothing.
    assert_eq!(
        get(&server, "/view/alice/brief/1", Some(&session)).status,
        200
    );
    let stranger = get(&server, "/view/alice/brief/1", None);
    assert_eq!(stranger.status, 200);
    assert!(
        stranger.body.contains("Sign in to view"),
        "{}",
        stranger.body
    );

    // A return address outside /view/ is not followed.
    let elsewhere = post(
        &server,
        "/account/release",
        &format!("csrf={csrf}&id=brief&visibility=public&version=1&return=https://evil.example/"),
        Some(&session),
    );
    assert_eq!(elsewhere.status, 200, "rendered the page, followed nothing");
}
