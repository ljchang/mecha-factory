//! `GET /@{handle}` — the generated index.
//!
//! The property the suite exists for: **the page is a view, never a
//! permission.** It shows what `inventory::Reach` already concluded is
//! public, and every way of not having a page answers identically — because
//! a 404 that distinguishes "disabled" from "no such person" is a directory
//! of who has an account here.

mod common;

use common::{Reply, Request, Server};
use mecha_factory::db::Scope;

fn get(server: &Server, target: &str) -> Reply {
    server.get(server.gate, target)
}

fn push(server: &Server, target: &str, body: &str) -> Reply {
    Request::new("PUT", target, server.gate.to_string())
        .auth(&server.key(Scope::Release))
        .body(body.to_string())
        .send(server.gate)
}

const ENABLED: &str = "enabled = true\ndisplay_name = \"Alice Chang\"\n\
                       tagline = \"Neuroscience\"\nlocation = \"Hanover\"\n\
                       timezone = \"America/New_York\"\n\
                       [[link]]\nkind = \"github\"\nurl = \"https://github.com/alice\"\n";

fn a_public_bundle(server: &Server, id: &str) {
    server
        .db
        .bundle_insert(&mecha_factory::db::BundleRow {
            user_id: server.user.id.clone(),
            id: id.into(),
            version: 1,
            digest: format!("d-{id}"),
            class: mecha_manifest::ContentClass::Static,
            title: format!("The {id}"),
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
            id,
            Some(1),
            mecha_manifest::Visibility::Public,
            &mecha_factory::db::now(),
        )
        .unwrap();
}

#[test]
fn an_enabled_hangar_shows_the_profile_and_what_is_public() {
    let server = common::start();
    assert_eq!(push(&server, "/v1/profile", ENABLED).status, 200);
    a_public_bundle(&server, "brief");

    let page = get(&server, "/@alice");
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("Alice Chang"), "{}", page.body);
    assert!(page.body.contains("Neuroscience"), "{}", page.body);
    assert!(page.body.contains("America/New_York"), "{}", page.body);
    assert!(
        page.body.contains("https://github.com/alice"),
        "{}",
        page.body
    );
    // The bundle line goes through the viewer, following the alias — never
    // to the artifact origin, and never version-pinned.
    assert!(
        page.body
            .contains(&format!("http://{}/view/alice/brief\"", server.gate)),
        "{}",
        page.body
    );
    assert!(!page.body.contains("/view/alice/brief/1"), "{}", page.body);
    assert!(
        !page.body.contains(&format!("alice.{}", server.artifacts)),
        "an artifact-origin link leaked onto the hangar: {}",
        page.body
    );
}

/// Four different ways of having no page, one answer. Byte for byte, because
/// any difference is the answer to the question the refusal is hiding.
#[test]
fn every_way_of_having_no_hangar_answers_identically() {
    let server = common::start();
    // Never enabled.
    let disabled = get(&server, "/@alice");
    // No such handle.
    let absent = get(&server, "/@nobody");
    assert_eq!(disabled.status, 404);
    assert_eq!(absent.status, 404);
    assert_eq!(disabled.body, absent.body, "the refusals differ");

    // Enabled, then suspended.
    assert_eq!(push(&server, "/v1/profile", ENABLED).status, 200);
    assert_eq!(get(&server, "/@alice").status, 200);
    server.db.user_status(&server.user.id, "suspended").unwrap();
    let suspended = get(&server, "/@alice");
    assert_eq!(suspended.status, 404);
    assert_eq!(
        suspended.body, absent.body,
        "a suspended user is distinguishable"
    );
}

/// The toggle is off until somebody says otherwise, so upgrading never gives
/// an existing account a public page it never asked for.
#[test]
fn a_profile_that_never_said_enabled_has_no_page() {
    let server = common::start();
    assert_eq!(
        push(&server, "/v1/profile", "display_name = \"Alice\"\n").status,
        200
    );
    assert_eq!(get(&server, "/@alice").status, 404);
}

/// The page must never name something a stranger cannot open. A private
/// bundle's *title* is the leak, well before its bytes are.
#[test]
fn a_private_or_withheld_bundle_is_not_named() {
    let server = common::start();
    assert_eq!(push(&server, "/v1/profile", ENABLED).status, 200);
    a_public_bundle(&server, "brief");
    a_public_bundle(&server, "secret");
    // One private, one withheld — the two ways a public-looking row is not.
    server
        .db
        .alias_set(
            &server.user.id,
            "secret",
            Some(1),
            mecha_manifest::Visibility::Private,
            &mecha_factory::db::now(),
        )
        .unwrap();
    a_public_bundle(&server, "pulled");
    server
        .db
        .bundle_withhold(
            &server.user.id,
            "pulled",
            1,
            Some("a complaint"),
            Some(&mecha_factory::db::now()),
        )
        .unwrap();

    let page = get(&server, "/@alice");
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("The brief"), "{}", page.body);
    assert!(
        !page.body.contains("The secret"),
        "a private bundle was named: {}",
        page.body
    );
    assert!(
        !page.body.contains("The pulled"),
        "a withheld bundle was named: {}",
        page.body
    );
}

/// The hangar's heading and intro come from its own record; its *lines* never
/// do.
#[test]
fn the_hangar_takes_its_heading_from_its_board() {
    let server = common::start();
    assert_eq!(push(&server, "/v1/profile", ENABLED).status, 200);
    assert_eq!(
        push(
            &server,
            "/v1/hangar",
            "heading = \"The Chang Lab\"\nintro = \"Everything in one place.\"\n"
        )
        .status,
        200
    );
    let page = get(&server, "/@alice");
    assert!(page.body.contains("The Chang Lab"), "{}", page.body);
    assert!(
        page.body.contains("Everything in one place."),
        "{}",
        page.body
    );
    // The identity block survives beneath it — the heading replaces the name,
    // not the profile.
    assert!(page.body.contains("Neuroscience"), "{}", page.body);
}

/// A stranger gets no sign-in box: this page goes in an email signature, and
/// offering an account to somebody who has none is noise. The mark stays,
/// because consistency across every page is the point of the shell.
#[test]
fn a_stranger_gets_the_chrome_but_nothing_to_sign_into() {
    let server = common::start();
    assert_eq!(push(&server, "/v1/profile", ENABLED).status, 200);
    let page = get(&server, "/@alice");
    assert!(
        !page.body.contains("Sign in"),
        "a stranger was offered a sign-in: {}",
        page.body
    );
    assert!(
        page.body.contains("<header class=\"site\"") || page.body.contains("class=\"mark\""),
        "the shared chrome is missing: {}",
        page.body
    );
}

/// Somebody who has only seen an artifact URL will try its root.
#[test]
fn the_artifact_origins_root_redirects_to_the_hangar() {
    let server = common::start();
    // The bare artifact origin belongs to nobody and still serves nothing —
    // there is no default tenant, for the same reason there is no default
    // origin.
    let reply = Request::new("GET", "/", server.artifacts.to_string()).send(server.artifacts);
    assert_eq!(reply.status, 404, "{}", reply.body);

    let reply = server.get(server.artifacts, "/");
    assert_eq!(reply.status, 302, "{}", reply.body);
    let location = reply.header("location").unwrap_or_default();
    assert!(
        location.contains("/@alice"),
        "no redirect to the hangar: {location:?}"
    );
}
