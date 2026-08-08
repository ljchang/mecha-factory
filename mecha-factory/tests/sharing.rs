//! Private sharing, against the real server.
//!
//! The property that carries the suite: **a grant names an address, and the
//! address is what gets in.** The mailed link is worthless without the
//! inbox, the artifact origin serves private bytes only to a capability the
//! gate minted after checking the grant, revoking the grant kills the bytes
//! mid-capability, and nothing on any path leaks whether a bundle exists to
//! somebody it was not shared with.

mod common;

use common::{start, Reply, Request, Server};
use mecha_factory::db::Scope;
use mecha_manifest::ContentClass;

fn get(server: &Server, target: &str, cookie: Option<(&str, &str)>) -> Reply {
    let mut request = Request::new("GET", target, server.gate.to_string());
    if let Some((name, value)) = cookie {
        request = request.header("Cookie", &format!("{name}={value}"));
    }
    request.send(server.gate)
}

fn post(server: &Server, target: &str, body: &str, cookie: Option<(&str, &str)>) -> Reply {
    let mut request = Request::new("POST", target, server.gate.to_string())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body.as_bytes().to_vec());
    if let Some((name, value)) = cookie {
        request = request.header("Cookie", &format!("{name}={value}"));
    }
    request.send(server.gate)
}

const TENANT: &str = "__Host-factory-session";
const READER: &str = "__Host-factory-viewer";

/// Publish `brief` v1 as alice and leave it private-but-live: aliased to
/// v1, visible to nobody — the state sharing exists for.
fn private_live_bundle(server: &Server) {
    let key = server.key(Scope::Publish);
    let reply = Request::new("POST", "/v1/bundles", server.gate.to_string())
        .auth(&key)
        .body(common::bundle_archive(
            "brief",
            1,
            ContentClass::Static,
            "Monday",
        ))
        .send(server.gate);
    assert_eq!(reply.status, 201, "{}", reply.body);
    server
        .db
        .alias_set(
            &server.user.id,
            "brief",
            Some(1),
            mecha_manifest::Visibility::Private,
            "2026-08-08T00:00:00Z",
        )
        .unwrap();
}

/// Sign alice in as a tenant and hand back the session token.
fn owner_signed_in(server: &Server) -> String {
    post(
        server,
        "/account/signin",
        "email=alice%40example.org",
        None,
    );
    let link = server.verification_token();
    let finished = post(server, &format!("/account/s/{link}"), "", None);
    assert_eq!(finished.status, 303, "{}", finished.head);
    finished
        .header("set-cookie")
        .unwrap()
        .split_once('=')
        .unwrap()
        .1
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

/// The CSRF value out of any page that embeds one.
fn csrf_of(page: &Reply) -> String {
    let marker = "name=\"csrf\" value=\"";
    let start = page.body.find(marker).expect("a csrf field") + marker.len();
    page.body[start..].split('"').next().unwrap().to_string()
}

/// The capability out of a viewer page's frame src.
fn cap_of(page: &Reply) -> String {
    let marker = "/g/";
    let start = page.body.find(marker).expect("a capability frame") + marker.len();
    page.body[start..].split('/').next().unwrap().to_string()
}

/// Share alice's `brief` with an address through the viewer's own form and
/// hand back the owner's session token.
fn shared_with(server: &Server, email: &str) -> String {
    let session = owner_signed_in(server);
    let page = get(server, "/view/alice/brief/1", Some((TENANT, &session)));
    assert_eq!(page.status, 200, "{}", page.body);
    let csrf = csrf_of(&page);
    let shared = post(
        server,
        "/account/share",
        &format!(
            "csrf={csrf}&id=brief&email={}&return=/view/alice/brief/1",
            email.replace('@', "%40")
        ),
        Some((TENANT, &session)),
    );
    assert_eq!(shared.status, 303, "{}", shared.head);
    session
}

/// Prove an inbox: run the reader sign-in for an address that has a grant,
/// returning the reader session token.
fn reader_signed_in(server: &Server, email: &str) -> String {
    let before = server.sent_links();
    let asked = post(
        server,
        "/view/signin",
        &format!(
            "email={}&return=/view/alice/brief",
            email.replace('@', "%40")
        ),
        None,
    );
    assert_eq!(asked.status, 200);
    assert!(asked.body.contains("Check your email"), "{}", asked.body);
    assert_eq!(server.sent_links(), before + 1, "a granted address is mailed");
    let link = server.last_link();
    let path = link.split(&server.gate.to_string()).nth(1).unwrap();
    // The person's path: interstitial first, then the button's POST.
    let interstitial = get(server, path, None);
    assert_eq!(interstitial.status, 200, "{}", interstitial.body);
    let finished = post(server, path, "", None);
    assert_eq!(finished.status, 303, "{}", finished.head);
    let cookie = finished.header("set-cookie").expect("a reader cookie");
    for needed in [READER, "HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
        assert!(cookie.contains(needed), "cookie lacks {needed}: {cookie}");
    }
    // The click lands back where the reader was going.
    assert_eq!(finished.header("location").unwrap(), "/view/alice/brief");
    cookie
        .split_once('=')
        .unwrap()
        .1
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

/// The whole arc: share, prove the inbox, read the bytes — while every
/// public path still answers 404 and an unshared address gets nothing.
#[test]
fn a_share_lets_exactly_the_named_address_read_a_private_bundle() {
    let server = start();
    private_live_bundle(&server);

    // Private means private: the world's paths all say nothing.
    assert_eq!(server.get(server.artifacts, "/b/brief/").status, 404);
    assert_eq!(server.get(server.artifacts, "/b/brief/v/1/").status, 404);

    // A visitor with no session gets the same sign-in page whether the
    // bundle exists or not.
    let real = get(&server, "/view/alice/brief/1", None);
    let fake = get(&server, "/view/nobody/nothing/1", None);
    assert_eq!(real.status, 200);
    assert_eq!(fake.status, 200);
    assert!(real.body.contains("Sign in to view"), "{}", real.body);
    assert!(fake.body.contains("Sign in to view"), "{}", fake.body);

    // The owner shares with casey; the box mails the bare viewer link.
    shared_with(&server, "casey@example.org");
    assert!(
        server.last_link().ends_with("/view/alice/brief"),
        "{}",
        server.last_link()
    );

    // An address nobody granted asks to sign in: same page, no mail.
    let before = server.sent_links();
    let unknown = post(
        &server,
        "/view/signin",
        "email=mallory%40example.org&return=/view/alice/brief",
        None,
    );
    assert!(unknown.body.contains("Check your email"), "{}", unknown.body);
    assert_eq!(server.sent_links(), before, "an unknown address sends nothing");

    // Casey proves the inbox and lands back on the bare link, which
    // resolves to the live version.
    let reader = reader_signed_in(&server, "casey@example.org");
    let resolved = get(&server, "/view/alice/brief", Some((READER, &reader)));
    assert_eq!(resolved.status, 303);
    assert_eq!(resolved.header("location").unwrap(), "/view/alice/brief/1");

    // The viewer page: a capability frame, the reader's identity, and no
    // owner controls — not disabled, absent.
    let page = get(&server, "/view/alice/brief/1", Some((READER, &reader)));
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("/g/"), "{}", page.body);
    assert!(page.body.contains("casey@example.org"), "{}", page.body);
    assert!(!page.body.contains("Manage"), "{}", page.body);

    // The capability serves the private bytes from the artifact origin —
    // uncacheable — while the public path still says nothing.
    let cap = cap_of(&page);
    let bytes = server.get(server.artifacts, &format!("/g/{cap}/"));
    assert_eq!(bytes.status, 200, "{}", bytes.body);
    assert!(bytes.body.contains("Monday"), "{}", bytes.body);
    assert!(
        bytes.header("cache-control").unwrap().contains("no-store"),
        "{}",
        bytes.head
    );
    assert_eq!(server.get(server.artifacts, "/b/brief/").status, 404);

    // A reader sees the released version only: v2 exists, the alias still
    // names v1, and casey's view of v2 is a plain 404.
    let key = server.key(Scope::Publish);
    Request::new("POST", "/v1/bundles", server.gate.to_string())
        .auth(&key)
        .body(common::bundle_archive(
            "brief",
            2,
            ContentClass::Static,
            "Tuesday",
        ))
        .send(server.gate);
    let sealed = get(&server, "/view/alice/brief/2", Some((READER, &reader)));
    assert_eq!(sealed.status, 404, "{}", sealed.body);
}

/// Revocation is immediate: the grant dies, and the capability minted under
/// it stops serving mid-lifetime.
#[test]
fn revoking_a_grant_kills_its_capability_at_the_next_fetch() {
    let server = start();
    private_live_bundle(&server);
    let session = shared_with(&server, "casey@example.org");
    let reader = reader_signed_in(&server, "casey@example.org");

    let page = get(&server, "/view/alice/brief/1", Some((READER, &reader)));
    let cap = cap_of(&page);
    assert_eq!(
        server.get(server.artifacts, &format!("/g/{cap}/")).status,
        200
    );

    // The owner unshares from the same Manage menu that shared.
    let owner_page = get(&server, "/view/alice/brief/1", Some((TENANT, &session)));
    assert!(
        owner_page.body.contains("Unshare casey@example.org"),
        "{}",
        owner_page.body
    );
    let share_id = server
        .db
        .shares_for_bundle(&server.user.id, "brief")
        .unwrap()[0]
        .id
        .clone();
    let csrf = csrf_of(&owner_page);
    let revoked = post(
        &server,
        "/account/share-revoke",
        &format!("csrf={csrf}&share={share_id}&return=/view/alice/brief/1"),
        Some((TENANT, &session)),
    );
    assert_eq!(revoked.status, 303, "{}", revoked.head);

    // The capability re-proves the grant at every use, so it is already
    // dead — no waiting out the token.
    assert_eq!(
        server.get(server.artifacts, &format!("/g/{cap}/")).status,
        404
    );
    // And the reader session, though alive, opens nothing.
    assert_eq!(
        get(&server, "/view/alice/brief/1", Some((READER, &reader))).status,
        404
    );
}

/// The owner's own preview rides the same capability path: a private
/// bundle's viewer frames bytes for its owner instead of the 404 the world
/// gets.
#[test]
fn the_owner_previews_a_private_bundle_through_a_capability() {
    let server = start();
    private_live_bundle(&server);
    let session = owner_signed_in(&server);
    let page = get(&server, "/view/alice/brief/1", Some((TENANT, &session)));
    assert_eq!(page.status, 200, "{}", page.body);
    let cap = cap_of(&page);
    let bytes = server.get(server.artifacts, &format!("/g/{cap}/"));
    assert_eq!(bytes.status, 200, "{}", bytes.body);
    assert!(bytes.body.contains("Monday"));
}

/// A tenant signed in with the granted address just works: the grant names
/// an email, and their session already proved one.
#[test]
fn a_tenant_session_with_the_granted_email_reads_without_a_reader_session() {
    let server = start();
    private_live_bundle(&server);
    let bob = server.add_user("bob");
    shared_with(&server, "bob@example.org");

    let token = mecha_factory::intake::mint_token();
    server
        .db
        .session_create(
            &bob.id,
            &mecha_factory::intake::hash_token(&token),
            &mecha_factory::db::now(),
            "2999-01-01T00:00:00Z",
        )
        .unwrap();
    let page = get(&server, "/view/alice/brief/1", Some((TENANT, &token)));
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("/g/"), "{}", page.body);
    assert!(!page.body.contains("Manage"), "{}", page.body);

    // And a tenant whose email nobody granted is a stranger with a name:
    // a plain 404, not a sign-in page.
    let carol = server.add_user("carol");
    let carol_token = mecha_factory::intake::mint_token();
    server
        .db
        .session_create(
            &carol.id,
            &mecha_factory::intake::hash_token(&carol_token),
            &mecha_factory::db::now(),
            "2999-01-01T00:00:00Z",
        )
        .unwrap();
    assert_eq!(
        get(&server, "/view/alice/brief/1", Some((TENANT, &carol_token))).status,
        404
    );
}

/// The reader sign-in budget holds, per address per day.
#[test]
fn reader_links_are_budgeted_per_address_per_day() {
    let server = start();
    private_live_bundle(&server);
    shared_with(&server, "casey@example.org");

    let before = server.sent_links();
    for _ in 0..mecha_factory::intake::VIEWER_LINKS_PER_DAY {
        post(
            &server,
            "/view/signin",
            "email=casey%40example.org&return=/view/alice/brief",
            None,
        );
    }
    assert_eq!(
        server.sent_links(),
        before + mecha_factory::intake::VIEWER_LINKS_PER_DAY as usize
    );
    let over = post(
        &server,
        "/view/signin",
        "email=casey%40example.org&return=/view/alice/brief",
        None,
    );
    assert_eq!(over.status, 200);
    assert!(over.body.contains("Check your email"));
    assert_eq!(
        server.sent_links(),
        before + mecha_factory::intake::VIEWER_LINKS_PER_DAY as usize,
        "the budget held"
    );
}

/// The reader session is a stranger at both other surfaces, and its
/// sign-out revokes the row.
#[test]
fn a_reader_session_opens_nothing_else_and_signs_out_server_side() {
    let server = start();
    private_live_bundle(&server);
    shared_with(&server, "casey@example.org");
    let reader = reader_signed_in(&server, "casey@example.org");

    // The reader token in the tenant's cookie: the sign-in form, no page.
    let crossed = get(&server, "/account", Some((TENANT, &reader)));
    assert!(crossed.body.contains("Sign in"), "{}", crossed.body);
    assert!(!crossed.body.contains("Machines"), "{}", crossed.body);

    // Sign out ends it server-side: the same token opens nothing after.
    let page = get(&server, "/view/alice/brief/1", Some((READER, &reader)));
    let csrf = csrf_of(&page);
    let out = post(
        &server,
        "/view/signout",
        &format!("csrf={csrf}"),
        Some((READER, &reader)),
    );
    assert_eq!(out.status, 303, "{}", out.head);
    let after = get(&server, "/view/alice/brief/1", Some((READER, &reader)));
    assert!(after.body.contains("Sign in to view"), "{}", after.body);
}
