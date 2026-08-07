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
