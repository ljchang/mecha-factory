//! The personal public surface's records, against the real server.
//!
//! Two properties carry the suite. **A push is `Scope::Release`**, because a
//! board is a page with buttons and that is squarely "what the world can
//! see" rather than "versions nobody can read". And **a push folds rather
//! than clobbers**: a field fixed in the cockpit survives a later push of a
//! file that did not touch it, which is the whole reason the baseline is
//! stored beside the record.

mod common;

use common::{Reply, Request, Server};
use mecha_factory::db::Scope;

fn put(server: &Server, target: &str, body: &str, scope: Scope) -> Reply {
    Request::new("PUT", target, server.gate.to_string())
        .auth(&server.key(scope))
        .body(body.to_string())
        .send(server.gate)
}

fn get(server: &Server, target: &str, scope: Scope) -> Reply {
    Request::new("GET", target, server.gate.to_string())
        .auth(&server.key(scope))
        .send(server.gate)
}

const PROFILE: &str =
    "# mine\nenabled = true\ntagline = \"Neuroscience\"\nlocation = \"Hanover\"\n";

#[test]
fn a_profile_pushes_and_pulls_back_verbatim() {
    let server = common::start();
    let reply = put(&server, "/v1/profile", PROFILE, Scope::Release);
    assert_eq!(reply.status, 200, "{}", reply.body);

    let reply = get(&server, "/v1/profile", Scope::Release);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(
        reply.json()["source"].as_str().unwrap(),
        PROFILE,
        "a pull must return the file, comments and all"
    );
    assert_eq!(reply.json()["drifted"], false);
}

/// Publishing versions nobody can read is one capability; putting a page at
/// a URL is another. A `Publish` key must not be able to do the second.
#[test]
fn a_publish_key_cannot_author_the_public_surface() {
    let server = common::start();
    for target in ["/v1/profile", "/v1/hangar", "/v1/boards/hello"] {
        let reply = put(&server, target, PROFILE, Scope::Publish);
        assert_eq!(
            reply.status, 403,
            "{target} accepted a publish key: {}",
            reply.body
        );
    }
}

/// The failure the merge exists to prevent, end to end: a tagline fixed in
/// the cockpit, then tonight's push of a file that never knew about it.
#[test]
fn a_cockpit_edit_survives_a_later_push_that_did_not_touch_it() {
    let server = common::start();
    assert_eq!(
        put(&server, "/v1/profile", PROFILE, Scope::Release).status,
        200
    );

    // The browser's half, straight at the store — the cockpit's editor is a
    // later step, and this is the write it will make.
    server
        .db
        .record_edit(
            &server.user.id,
            mecha_factory::db::RECORD_PROFILE,
            "",
            "enabled = true\ntagline = \"Fixed on my phone\"\nlocation = \"Hanover\"\n",
            &mecha_factory::db::now(),
        )
        .unwrap();

    // The same file as before, plus one changed field it does know about.
    let pushed = "# mine\nenabled = true\ntagline = \"Neuroscience\"\nlocation = \"Lebanon\"\n";
    let reply = put(&server, "/v1/profile", pushed, Scope::Release);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(
        reply.json()["overwritten"].as_array().unwrap().is_empty(),
        "nothing was in conflict: {}",
        reply.body
    );

    let source = get(&server, "/v1/profile", Scope::Release).json()["source"]
        .as_str()
        .unwrap()
        .to_string();
    let profile = mecha_manifest::Profile::from_toml(&source).unwrap();
    assert_eq!(
        profile.tagline.as_deref(),
        Some("Fixed on my phone"),
        "the push reverted an edit it never knew about"
    );
    assert_eq!(profile.location.as_deref(), Some("Lebanon"));
}

/// A genuine conflict goes to the file — and the push says so, rather than
/// letting a near miss pass unremarked.
#[test]
fn a_conflicting_push_wins_and_names_what_it_overwrote() {
    let server = common::start();
    assert_eq!(
        put(&server, "/v1/profile", PROFILE, Scope::Release).status,
        200
    );
    server
        .db
        .record_edit(
            &server.user.id,
            mecha_factory::db::RECORD_PROFILE,
            "",
            "enabled = true\ntagline = \"From the browser\"\nlocation = \"Hanover\"\n",
            &mecha_factory::db::now(),
        )
        .unwrap();

    let pushed = "enabled = true\ntagline = \"From the file\"\nlocation = \"Hanover\"\n";
    let reply = put(&server, "/v1/profile", pushed, Scope::Release);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(
        reply.json()["overwritten"],
        serde_json::json!(["tagline"]),
        "{}",
        reply.body
    );
    assert!(
        reply.json()["note"].as_str().unwrap().contains("pull"),
        "{}",
        reply.body
    );
    assert_eq!(
        get(&server, "/v1/profile", Scope::Release).json()["drifted"],
        false
    );
}

#[test]
fn a_board_names_itself_and_the_url_must_agree() {
    let server = common::start();
    let good = "slug = \"hello\"\nheading = \"Get in touch\"\n\
                [[entry]]\nkind = \"form\"\nid = \"letter\"\nlabel = \"Write\"\n";
    assert_eq!(
        put(&server, "/v1/boards/hello", good, Scope::Release).status,
        200
    );

    let reply = put(&server, "/v1/boards/teaching", good, Scope::Release);
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(reply.body.contains("calls itself"), "{}", reply.body);

    // The hangar is the board with no slug, and it declares no lines.
    assert_eq!(
        put(
            &server,
            "/v1/hangar",
            "heading = \"Alice\"\n",
            Scope::Release
        )
        .status,
        200
    );
    let reply = put(&server, "/v1/hangar", good, Scope::Release);
    assert_eq!(reply.status, 400, "{}", reply.body);
}

/// A slug goes in an email signature and can never be taken back, so the
/// gate's own route names are refused before the first one exists.
#[test]
fn a_reserved_slug_is_refused_at_the_endpoint() {
    let server = common::start();
    for slug in ["v", "account", "view", "slides"] {
        let body = format!("slug = \"{slug}\"\n");
        let reply = put(
            &server,
            &format!("/v1/boards/{slug}"),
            &body,
            Scope::Release,
        );
        assert_eq!(reply.status, 400, "`{slug}` was accepted: {}", reply.body);
        assert!(reply.body.contains("reserved"), "{}", reply.body);
    }
}

/// A machine cannot pull a board it has never heard of, so it has to be able
/// to ask what exists.
#[test]
fn boards_are_listable_and_a_missing_one_is_not_an_error() {
    let server = common::start();
    assert_eq!(
        put(
            &server,
            "/v1/boards/hello",
            "slug = \"hello\"\n",
            Scope::Release
        )
        .status,
        200
    );
    let reply = get(&server, "/v1/boards", Scope::Release);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let boards = reply.json()["boards"].as_array().unwrap().clone();
    assert_eq!(boards.len(), 1);
    assert_eq!(boards[0]["slug"], "hello");

    // Pulling one that was never pushed is how a fresh machine starts, not a
    // failure.
    let reply = get(&server, "/v1/boards/teaching", Scope::Release);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["source"], "");
}

/// A URL that a browser would execute must never survive to a rendered page.
#[test]
fn a_javascript_url_is_refused_at_the_edge() {
    let server = common::start();
    let reply = put(
        &server,
        "/v1/profile",
        "[[link]]\nurl = \"javascript:alert(1)\"\n",
        Scope::Release,
    );
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(reply.body.contains("http(s)"), "{}", reply.body);
}

/// Two valid texts can merge into an invalid one — a hangar's heading pushed
/// while the browser added lines to it. The check runs on the merged text,
/// inside the same lock that stores it, so nothing unrenderable lands.
#[test]
fn a_merge_that_would_not_validate_is_refused_rather_than_stored() {
    let server = common::start();
    assert_eq!(
        put(
            &server,
            "/v1/hangar",
            "heading = \"Alice\"\n",
            Scope::Release
        )
        .status,
        200
    );
    // The browser adds lines: legal for a switchboard, not for a hangar, but
    // it goes in under the store's own write.
    server
        .db
        .record_edit(
            &server.user.id,
            mecha_factory::db::RECORD_BOARD,
            "",
            "heading = \"Alice\"\n[[entry]]\nkind = \"link\"\nurl = \"https://a.example\"\nlabel = \"A\"\n",
            &mecha_factory::db::now(),
        )
        .unwrap();

    let reply = put(
        &server,
        "/v1/hangar",
        "heading = \"Alice Chang\"\n",
        Scope::Release,
    );
    assert_eq!(reply.status, 409, "{}", reply.body);
    // And the bad merge was not stored: the record still reads as it did.
    let source = get(&server, "/v1/hangar", Scope::Release).json()["source"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        source.contains("a.example"),
        "the store was mutated: {source}"
    );
}
