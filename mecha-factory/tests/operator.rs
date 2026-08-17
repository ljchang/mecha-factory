//! The operator surface, against the real server.
//!
//! The property that carries it: **the two surfaces are kept apart by the
//! credential.** No tenant key reaches an admin endpoint, the operate key is
//! refused everywhere a tenant key works, and the admin verbs do exactly
//! what the on-box CLI did — which is what lets the SSH session retire.

mod common;

use common::{Reply, Request, Server};
use mecha_factory::db::Scope;

fn operator_key(server: &Server) -> String {
    mecha_factory::keys::mint(&server.db, "", Scope::Operate, "test-op")
        .unwrap()
        .token
}

fn admin_get(server: &Server, target: &str, key: &str) -> Reply {
    Request::new("GET", target, server.gate.to_string())
        .auth(key)
        .send(server.gate)
}

fn admin_post(server: &Server, target: &str, key: &str, body: serde_json::Value) -> Reply {
    Request::new("POST", target, server.gate.to_string())
        .auth(key)
        .header("Content-Type", "application/json")
        .body(body.to_string().into_bytes())
        .send(server.gate)
}

/// The credential is the boundary: tenant keys die at the admin door, the
/// operate key dies at the tenant door, and nothing gets in with neither.
#[test]
fn the_two_surfaces_are_kept_apart_by_the_credential() {
    let server = common::start();
    let operator = operator_key(&server);
    let tenant = server.key(Scope::Publish);

    assert_eq!(admin_get(&server, "/v1/admin/users", &operator).status, 200);
    assert_eq!(
        admin_get(&server, "/v1/admin/users", &tenant).status,
        403,
        "a tenant key must not operate"
    );
    let bare = Request::new("GET", "/v1/admin/users", server.gate.to_string()).send(server.gate);
    assert_eq!(bare.status, 401);

    // And the other direction: the operator's key publishes nothing.
    let crossed = Request::new("GET", "/v1/types", server.gate.to_string())
        .auth(&operator)
        .send(server.gate);
    assert_eq!(crossed.status, 403, "{}", crossed.body);
}

/// Suspension from afar is the abuse response: the account's keys stop, and
/// restore brings them back — the same rows the on-box CLI drives.
#[test]
fn an_operator_suspends_and_restores_from_afar() {
    let server = common::start();
    let operator = operator_key(&server);
    let tenant = server.key(Scope::Publish);

    let off = admin_post(
        &server,
        "/v1/admin/users/alice/status",
        &operator,
        serde_json::json!({ "status": "suspended" }),
    );
    assert_eq!(off.status, 200, "{}", off.body);
    let dead = Request::new("GET", "/v1/types", server.gate.to_string())
        .auth(&tenant)
        .send(server.gate);
    assert_eq!(dead.status, 403, "{}", dead.body);

    let on = admin_post(
        &server,
        "/v1/admin/users/alice/status",
        &operator,
        serde_json::json!({ "status": "active" }),
    );
    assert_eq!(on.status, 200);
    let alive = Request::new("GET", "/v1/types", server.gate.to_string())
        .auth(&tenant)
        .send(server.gate);
    assert_eq!(alive.status, 200, "{}", alive.body);

    // Only the two real statuses exist over the wire.
    let nonsense = admin_post(
        &server,
        "/v1/admin/users/alice/status",
        &operator,
        serde_json::json!({ "status": "deleted" }),
    );
    assert_eq!(nonsense.status, 400);
}

/// An invite minted from afar is a real invite: the box mails the link, and
/// the link claims a handle exactly as one minted on the box does.
#[test]
fn a_remote_invite_is_mailed_by_the_box_and_claims_a_handle() {
    let server = common::start();
    let operator = operator_key(&server);

    let minted = admin_post(
        &server,
        "/v1/admin/invites",
        &operator,
        serde_json::json!({ "email": "casey@example.org", "note": "from afar" }),
    );
    assert_eq!(minted.status, 200, "{}", minted.body);
    let link = minted.json()["link"].as_str().unwrap().to_string();
    // The mailer saw the same link the operator was handed.
    assert_eq!(
        server.verification_token(),
        link.rsplit('/').next().unwrap()
    );
    let token = link.rsplit('/').next().unwrap();

    let claim = Request::new("POST", &format!("/signup/{token}"), server.gate.to_string())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(b"handle=casey".to_vec())
        .send(server.gate);
    assert_eq!(claim.status, 200, "{}", claim.body);
    assert!(server.db.user_by_handle("casey").unwrap().is_some());

    // The ledger shows it, remotely.
    let listed = admin_get(&server, "/v1/admin/invites", &operator);
    assert!(listed.body.contains("claimed"), "{}", listed.body);
}

/// Break-glass: the operator kills any key, and the row survives to say so.
#[test]
fn an_operator_revokes_any_key_and_sees_them_all() {
    let server = common::start();
    let operator = operator_key(&server);
    let minted =
        mecha_factory::keys::mint(&server.db, &server.user.id, Scope::Publish, "laptop").unwrap();

    let listed = admin_get(&server, "/v1/admin/keys", &operator);
    assert_eq!(listed.status, 200);
    assert!(listed.body.contains("laptop"), "{}", listed.body);
    assert!(listed.body.contains("(operator)"), "{}", listed.body);

    let revoked = admin_post(
        &server,
        &format!("/v1/admin/keys/{}/revoke", minted.row.id),
        &operator,
        serde_json::json!({}),
    );
    assert_eq!(revoked.status, 200);
    assert_eq!(revoked.json()["revoked"], true);
    let dead = Request::new("GET", "/v1/types", server.gate.to_string())
        .auth(&minted.token)
        .send(server.gate);
    assert_eq!(dead.status, 401);
}

// ---- the browser panel --------------------------------------------------
//
// The property the page tests carry: **the way in is the CLI, and the
// session is a stranger to tenant sessions.** The operate key mints a
// one-time URL; the URL becomes its own cookie against its own tables; and
// neither cookie means anything at the other surface.

fn panel_get(server: &Server, target: &str, cookie: Option<&str>) -> Reply {
    let mut request = Request::new("GET", target, server.gate.to_string());
    if let Some(cookie) = cookie {
        request = request.header("Cookie", &format!("__Host-factory-operator={cookie}"));
    }
    request.send(server.gate)
}

fn panel_post(server: &Server, target: &str, body: &str, cookie: Option<&str>) -> Reply {
    let mut request = Request::new("POST", target, server.gate.to_string())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body.as_bytes().to_vec());
    if let Some(cookie) = cookie {
        request = request.header("Cookie", &format!("__Host-factory-operator={cookie}"));
    }
    request.send(server.gate)
}

/// The whole way in: the key asks for a URL, the URL's POST becomes the
/// session. Returns the session token.
fn panel_signed_in(server: &Server, operator: &str) -> String {
    let minted = admin_post(server, "/v1/admin/signin", operator, serde_json::json!({}));
    assert_eq!(minted.status, 200, "{}", minted.body);
    let url = minted.json()["url"].as_str().unwrap().to_string();
    let token = url.rsplit('/').next().unwrap().to_string();
    // The person's path: interstitial first, which spends nothing.
    let interstitial = panel_get(server, &format!("/admin/s/{token}"), None);
    assert_eq!(interstitial.status, 200, "{}", interstitial.body);
    let finished = panel_post(server, &format!("/admin/s/{token}"), "", None);
    assert_eq!(finished.status, 303, "{}", finished.head);
    let cookie = finished.header("set-cookie").expect("a session cookie");
    for needed in [
        "__Host-factory-operator",
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
        "Path=/",
    ] {
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

/// The CSRF value the panel embeds.
fn panel_csrf(server: &Server, session: &str) -> String {
    let page = panel_get(server, "/admin", Some(session));
    assert_eq!(page.status, 200, "{}", page.body);
    let marker = "name=\"csrf\" value=\"";
    let start = page.body.find(marker).expect("a csrf field") + marker.len();
    page.body[start..].split('"').next().unwrap().to_string()
}

/// The way in is the CLI: a signed-out `/admin` has instructions and no
/// form, a tenant key cannot mint the link, and the link works exactly once.
#[test]
fn the_panel_signs_in_from_a_cli_minted_link_once() {
    let server = common::start();
    let operator = operator_key(&server);

    // Signed out: told to run the CLI, offered nothing to type into.
    let out = panel_get(&server, "/admin", None);
    assert_eq!(out.status, 200);
    assert!(
        out.body.contains("factory-publish operator signin"),
        "{}",
        out.body
    );
    assert!(!out.body.contains("<input"), "{}", out.body);

    // A tenant key does not get a link.
    let tenant = server.key(Scope::Publish);
    let refused = admin_post(&server, "/v1/admin/signin", &tenant, serde_json::json!({}));
    assert_eq!(refused.status, 403, "{}", refused.body);

    let minted = admin_post(
        &server,
        "/v1/admin/signin",
        &operator,
        serde_json::json!({}),
    );
    assert_eq!(minted.status, 200);
    let url = minted.json()["url"].as_str().unwrap().to_string();
    let token = url.rsplit('/').next().unwrap().to_string();

    // Spend the minted link: interstitial spends nothing, the POST does.
    let interstitial = panel_get(&server, &format!("/admin/s/{token}"), None);
    assert_eq!(interstitial.status, 200, "{}", interstitial.body);
    let finished = panel_post(&server, &format!("/admin/s/{token}"), "", None);
    assert_eq!(finished.status, 303, "{}", finished.head);
    let session = finished
        .header("set-cookie")
        .expect("a session cookie")
        .split_once('=')
        .unwrap()
        .1
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let page = panel_get(&server, "/admin", Some(&session));
    assert_eq!(page.status, 200);
    assert!(page.body.contains("Accounts"), "{}", page.body);
    assert!(page.body.contains("alice"), "{}", page.body);

    // The spent link is any dead token.
    let again = panel_post(&server, &format!("/admin/s/{token}"), "", None);
    assert_eq!(again.status, 404, "{}", again.head);
}

/// Neither session means anything at the other surface: the cookies are
/// different names, the tables are different tables, and a token from one
/// presented as the other is nobody.
#[test]
fn the_operator_session_and_a_tenant_session_are_strangers() {
    let server = common::start();
    let operator = operator_key(&server);
    let op_session = panel_signed_in(&server, &operator);

    // A real tenant session, planted the way sign-in plants one.
    let tenant_token = mecha_factory::intake::mint_token();
    server
        .db
        .session_create(
            &server.user.id,
            &mecha_factory::intake::hash_token(&tenant_token),
            &mecha_factory::db::now(),
            "2999-01-01T00:00:00Z",
        )
        .unwrap();

    // The tenant token in the operator's cookie: the instructions page.
    let crossed = panel_get(&server, "/admin", Some(&tenant_token));
    assert!(
        crossed.body.contains("factory-publish operator signin"),
        "{}",
        crossed.body
    );
    assert!(!crossed.body.contains("Accounts"), "{}", crossed.body);

    // The operator token in the tenant's cookie: the sign-in form, and no
    // account page — there is no user behind an operator session to find.
    let reply = Request::new("GET", "/account", server.gate.to_string())
        .header("Cookie", &format!("__Host-factory-session={op_session}"))
        .send(server.gate);
    assert!(reply.body.contains("Sign in"), "{}", reply.body);
    assert!(!reply.body.contains("Machines"), "{}", reply.body);
}

/// The panel's verbs drive the same rows the JSON endpoints drive, behind
/// the session's CSRF — and a stale form changes nothing.
#[test]
fn the_panel_suspends_invites_and_withholds_with_csrf() {
    let server = common::start();
    let operator = operator_key(&server);
    let session = panel_signed_in(&server, &operator);
    let csrf = panel_csrf(&server, &session);

    // Wrong CSRF: refused, nothing moves.
    let forged = panel_post(
        &server,
        "/admin/status",
        "csrf=wrong&handle=alice&status=suspended",
        Some(&session),
    );
    assert_eq!(forged.status, 403, "{}", forged.body);
    assert!(server.db.user_by_handle("alice").unwrap().unwrap().active());

    // Suspend, then restore — the same rows the CLI drives.
    let off = panel_post(
        &server,
        "/admin/status",
        &format!("csrf={csrf}&handle=alice&status=suspended"),
        Some(&session),
    );
    assert_eq!(off.status, 200, "{}", off.body);
    assert!(!server.db.user_by_handle("alice").unwrap().unwrap().active());
    let on = panel_post(
        &server,
        "/admin/status",
        &format!("csrf={csrf}&handle=alice&status=active"),
        Some(&session),
    );
    assert_eq!(on.status, 200);
    assert!(server.db.user_by_handle("alice").unwrap().unwrap().active());

    // An invite minted from the page is mailed by the box and shows on the
    // panel; revoking it kills the link.
    let minted = panel_post(
        &server,
        "/admin/invite",
        &format!("csrf={csrf}&email=casey%40example.org&note=from+the+panel"),
        Some(&session),
    );
    // Post-redirect-get: minting mails a stranger, so a refresh of the
    // response must re-submit nothing.
    assert_eq!(minted.status, 303, "{}", minted.head);
    let panel = panel_get(&server, "/admin", Some(&session));
    assert!(panel.body.contains("casey@example.org"), "{}", panel.body);
    let invites = server.db.invites().unwrap();
    assert_eq!(invites.len(), 1);
    let revoked = panel_post(
        &server,
        "/admin/invite-revoke",
        &format!("csrf={csrf}&id={}", invites[0].id),
        Some(&session),
    );
    assert!(revoked.body.contains("no longer works"), "{}", revoked.body);

    // Withhold from the form; the row appears with a Restore button; undo
    // through it puts the version back.
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
    let withheld = panel_post(
        &server,
        "/admin/withhold",
        &format!("csrf={csrf}&handle=alice&id=brief&version=1&reason=reported"),
        Some(&session),
    );
    assert_eq!(withheld.status, 200, "{}", withheld.body);
    assert!(server
        .db
        .bundle(&server.user.id, "brief", 1)
        .unwrap()
        .unwrap()
        .withheld_at
        .is_some());
    assert!(withheld.body.contains("reported"), "{}", withheld.body);
    let restored = panel_post(
        &server,
        "/admin/withhold",
        &format!("csrf={csrf}&handle=alice&id=brief&version=1&undo=1"),
        Some(&session),
    );
    assert_eq!(restored.status, 200);
    assert!(server
        .db
        .bundle(&server.user.id, "brief", 1)
        .unwrap()
        .unwrap()
        .withheld_at
        .is_none());
}

/// Break-glass ends the browser too: the session rides its key, so revoking
/// the key — even from the panel it signed in — is the last thing the
/// session does.
#[test]
fn the_panel_session_dies_with_its_key() {
    let server = common::start();
    let operator = operator_key(&server);
    let session = panel_signed_in(&server, &operator);
    let csrf = panel_csrf(&server, &session);

    let key_id = server
        .db
        .keys()
        .unwrap()
        .into_iter()
        .find(|k| k.scope == Scope::Operate)
        .unwrap()
        .id;
    let last = panel_post(
        &server,
        "/admin/key-revoke",
        &format!("csrf={csrf}&id={key_id}"),
        Some(&session),
    );
    assert_eq!(last.status, 200, "{}", last.body);

    // The very next look: signed out, structurally.
    let after = panel_get(&server, "/admin", Some(&session));
    assert!(
        after.body.contains("factory-publish operator signin"),
        "{}",
        after.body
    );
    // And the dead key mints no more links.
    let refused = admin_post(
        &server,
        "/v1/admin/signin",
        &operator,
        serde_json::json!({}),
    );
    assert_eq!(refused.status, 401, "{}", refused.body);
}

/// Sign-out revokes the row, not just the cookie.
#[test]
fn the_panel_signs_out_server_side() {
    let server = common::start();
    let operator = operator_key(&server);
    let session = panel_signed_in(&server, &operator);
    let csrf = panel_csrf(&server, &session);
    let out = panel_post(
        &server,
        "/admin/signout",
        &format!("csrf={csrf}"),
        Some(&session),
    );
    assert_eq!(out.status, 303);
    let after = panel_get(&server, "/admin", Some(&session));
    assert!(
        after.body.contains("factory-publish operator signin"),
        "{}",
        after.body
    );
}

/// Withholding from afar flips the same reversible switch the CLI does.
#[test]
fn an_operator_withholds_and_restores_a_version() {
    let server = common::start();
    let operator = operator_key(&server);
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

    let withheld = admin_post(
        &server,
        "/v1/admin/withhold",
        &operator,
        serde_json::json!({ "handle": "alice", "id": "brief", "version": 1,
                            "reason": "reported" }),
    );
    assert_eq!(withheld.status, 200, "{}", withheld.body);
    let row = server
        .db
        .bundle(&server.user.id, "brief", 1)
        .unwrap()
        .unwrap();
    assert!(row.withheld_at.is_some());

    let restored = admin_post(
        &server,
        "/v1/admin/withhold",
        &operator,
        serde_json::json!({ "handle": "alice", "id": "brief", "version": 1, "undo": true }),
    );
    assert_eq!(restored.status, 200);
    let row = server
        .db
        .bundle(&server.user.id, "brief", 1)
        .unwrap()
        .unwrap();
    assert!(row.withheld_at.is_none());
}

// ---- the email door ------------------------------------------------------
//
// The second way in: one button, the address is config's, and the emailed
// link redeems into the same session the CLI link does — anchored to the
// `email-door` key row, whose revocation is the door's kill switch.

/// The whole email path: button → mailed link → interstitial → session,
/// with no operate key anywhere near a browser, and the link dead after one
/// use.
#[test]
fn the_email_door_signs_in_without_the_key() {
    let server = common::start();

    // Signed out, the page offers the button beside the CLI instructions.
    let out = panel_get(&server, "/admin", None);
    assert!(out.body.contains("Email me a sign-in link"), "{}", out.body);

    let asked = panel_post(&server, "/admin/signin", "", None);
    assert_eq!(asked.status, 200, "{}", asked.body);
    assert_eq!(server.sent_links(), 1, "one link mailed");
    let link = server.last_link();
    let token = link.rsplit('/').next().unwrap().to_string();

    // The person's path: interstitial spends nothing, the POST signs in.
    let interstitial = panel_get(&server, &format!("/admin/s/{token}"), None);
    assert_eq!(interstitial.status, 200, "{}", interstitial.body);
    let finished = panel_post(&server, &format!("/admin/s/{token}"), "", None);
    assert_eq!(finished.status, 303, "{}", finished.head);
    let session = finished
        .header("set-cookie")
        .expect("a session cookie")
        .split_once('=')
        .unwrap()
        .1
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let page = panel_get(&server, "/admin", Some(&session));
    assert_eq!(page.status, 200);
    assert!(page.body.contains("email-door"), "{}", page.body);
    assert!(page.body.contains("Accounts"), "{}", page.body);

    // A refresh of the panel re-extends the cookie — the rolling session.
    assert!(
        page.header("set-cookie")
            .expect("a refreshed cookie")
            .contains("Max-Age"),
        "{}",
        page.head
    );

    // The spent link is any dead token.
    let again = panel_post(&server, &format!("/admin/s/{token}"), "", None);
    assert_eq!(again.status, 404, "{}", again.head);
}

/// The button answers with one page whatever happened, and stops mailing at
/// the daily budget — the two properties that keep a public button from
/// being an oracle or a mail cannon.
#[test]
fn the_email_door_is_budgeted_and_answers_identically() {
    let server = common::start();

    let first = panel_post(&server, "/admin/signin", "", None);
    assert_eq!(first.status, 200);
    for _ in 1..mecha_factory::http::admin::EMAIL_LINKS_PER_DAY {
        panel_post(&server, "/admin/signin", "", None);
    }
    assert_eq!(
        server.sent_links() as i64,
        mecha_factory::http::admin::EMAIL_LINKS_PER_DAY
    );

    // Over budget: the same bytes, and nothing sent.
    let over = panel_post(&server, "/admin/signin", "", None);
    assert_eq!(over.status, 200);
    assert_eq!(over.body, first.body, "the page must not become an oracle");
    assert_eq!(
        server.sent_links() as i64,
        mecha_factory::http::admin::EMAIL_LINKS_PER_DAY,
        "over-budget clicks mail nothing"
    );
}

/// Break-glass revoking the `email-door` key is the door's kill switch: the
/// button goes quiet, existing email sessions die, and nothing here ever
/// resurrects the row.
#[test]
fn revoking_the_email_door_key_closes_the_door_and_its_sessions() {
    let server = common::start();

    // Sign in through the door first, so there is a session to kill.
    panel_post(&server, "/admin/signin", "", None);
    let token = server.last_link().rsplit('/').next().unwrap().to_string();
    let finished = panel_post(&server, &format!("/admin/s/{token}"), "", None);
    let session = finished
        .header("set-cookie")
        .expect("a session cookie")
        .split_once('=')
        .unwrap()
        .1
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert_eq!(panel_get(&server, "/admin", Some(&session)).status, 200);

    server
        .db
        .key_revoke(mecha_factory::db::EMAIL_DOOR_KEY, "2026-08-08T00:00:00Z")
        .unwrap();

    // The session died with its key…
    let after = panel_get(&server, "/admin", Some(&session));
    assert!(
        after.body.contains("factory-publish operator signin"),
        "{}",
        after.body
    );
    // …and the button mails nothing, while answering the same page.
    let sent_before = server.sent_links();
    let quiet = panel_post(&server, "/admin/signin", "", None);
    assert_eq!(quiet.status, 200);
    assert_eq!(server.sent_links(), sent_before, "a revoked door is closed");
}

/// The approval gate, end to end: an ask waits, the panel shows it beside
/// the budget, denying closes it silently, approving mints and mails the
/// one invite definition — and a spent week refuses the approval while the
/// request keeps waiting.
#[test]
fn the_panel_approves_and_denies_requests_with_the_budget_in_view() {
    let server = common::start();
    let operator = operator_key(&server);
    let session = panel_signed_in(&server, &operator);

    // Two strangers ask at the door.
    for email in ["casey%40example.org", "dana%40example.org"] {
        let asked = Request::new("POST", "/signup", server.gate.to_string())
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!("email={email}").into_bytes())
            .send(server.gate);
        assert_eq!(asked.status, 200, "{}", asked.head);
    }
    assert_eq!(server.sent_links(), 0, "asks mail nothing");

    // The panel shows the queue and the budget in the same breath: the
    // number the operator approves against is computed by the same function
    // the approval handler enforces.
    let panel = panel_get(&server, "/admin", Some(&session));
    assert!(panel.body.contains("Requests"), "{}", panel.body);
    assert!(panel.body.contains("casey@example.org"), "{}", panel.body);
    assert!(panel.body.contains("dana@example.org"), "{}", panel.body);
    assert!(
        panel.body.contains("of 40 certificate slots"),
        "{}",
        panel.body
    );

    let csrf = panel_csrf(&server, &session);
    let pending = server.db.asks_pending().unwrap();
    let casey = pending.iter().find(|r| r.email.contains("casey")).unwrap();
    let dana = pending.iter().find(|r| r.email.contains("dana")).unwrap();

    // Denied: closed, silent, and gone from the queue.
    let denied = panel_post(
        &server,
        "/admin/ask-deny",
        &format!("csrf={csrf}&id={}", dana.id),
        Some(&session),
    );
    assert_eq!(denied.status, 200, "{}", denied.head);
    assert!(denied.body.contains("Request denied"), "{}", denied.body);
    assert_eq!(server.sent_links(), 0, "a denial mails nothing");
    assert_eq!(server.db.asks_pending().unwrap().len(), 1);

    // Approved: the invite exists through the one mint definition, the mail
    // went, and the request is decided.
    let approved = panel_post(
        &server,
        "/admin/ask-approve",
        &format!("csrf={csrf}&id={}", casey.id),
        Some(&session),
    );
    assert_eq!(approved.status, 303, "{}", approved.head);
    assert_eq!(server.sent_links(), 1, "approval is what mails");
    let invites = server.db.invites().unwrap();
    let row = invites
        .iter()
        .find(|r| r.email == "casey@example.org")
        .expect("the approved invite");
    assert_eq!(row.note, "approved request");
    assert!(server.db.asks_pending().unwrap().is_empty());

    // A spent week refuses the approval and keeps the request pending.
    let now = mecha_factory::db::now();
    for n in 0..40 {
        server
            .db
            .user_create(&format!("w{n}"), &format!("w{n}@example.org"), &now)
            .unwrap();
    }
    let late = Request::new("POST", "/signup", server.gate.to_string())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(b"email=late%40example.org".to_vec())
        .send(server.gate);
    assert_eq!(late.status, 200, "{}", late.head);
    let late_row = &server.db.asks_pending().unwrap()[0];
    let refused = panel_post(
        &server,
        "/admin/ask-approve",
        &format!("csrf={csrf}&id={}", late_row.id),
        Some(&session),
    );
    assert_eq!(refused.status, 200, "{}", refused.head);
    assert!(
        refused.body.contains("No certificate slots free"),
        "{}",
        refused.body
    );
    assert_eq!(server.sent_links(), 1, "a refused approval mails nothing");
    assert_eq!(server.db.asks_pending().unwrap().len(), 1, "still waiting");
}
