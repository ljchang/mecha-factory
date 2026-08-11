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

/// A session cookie for the default user, by walking the path they would.
fn signed_in(server: &Server) -> String {
    Request::new("POST", "/account/signin", server.gate.to_string())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("email=alice%40example.org".to_string())
        .send(server.gate);
    let token = server.verification_token();
    let finished = Request::new(
        "POST",
        &format!("/account/s/{token}"),
        server.gate.to_string(),
    )
    .header("Content-Type", "application/x-www-form-urlencoded")
    .body(String::new())
    .send(server.gate);
    let cookie = finished.header("set-cookie").expect("a session cookie");
    cookie
        .split_once('=')
        .unwrap()
        .1
        .split(';')
        .next()
        .unwrap()
        .to_string()
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

// ---- switchboards -------------------------------------------------------

fn a_form(server: &Server, id: &str) {
    let manifest = format!(
        "id = \"{id}\"\nversion = 1\ntitle = \"{id} title\"\n\
         [[fields]]\nname = \"who\"\nlabel = \"You\"\nkind = \"text\"\nmax_length = 80\nrequired = true\n\
         [[fields]]\nname = \"reply_to\"\nlabel = \"Email\"\nkind = \"email\"\nrequired = true\n\
         [verification]\nfield = \"reply_to\"\n"
    );
    let parsed = mecha_manifest::RequestType::from_toml(&manifest).unwrap();
    server
        .db
        .type_put(&mecha_factory::db::TypeRow {
            user_id: server.user.id.clone(),
            id: id.into(),
            title: parsed.title.clone(),
            manifest,
            schema: "{}".into(),
            updated_at: mecha_factory::db::now(),
        })
        .unwrap();
}

const HELLO: &str = "slug = \"hello\"\nheading = \"Get in touch\"\n\
                     intro = \"The fastest way to reach me.\"\n\
                     [[entry]]\nkind = \"form\"\nid = \"letter\"\nlabel = \"Request a letter\"\n\
                     blurb = \"Three weeks, please.\"\n\
                     [[entry]]\nkind = \"link\"\nurl = \"https://cosanlab.com/x\"\nlabel = \"Lab website\"\n";

#[test]
fn a_switchboard_renders_its_patched_lines() {
    let server = common::start();
    a_form(&server, "letter");
    assert_eq!(push(&server, "/v1/boards/hello", HELLO).status, 200);

    let page = get(&server, "/@alice/hello");
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("Get in touch"), "{}", page.body);
    assert!(page.body.contains("Three weeks, please."), "{}", page.body);
    assert!(
        page.body
            .contains(&format!("http://{}/f/alice/letter", server.gate)),
        "{}",
        page.body
    );
    // An external line shows its host, always: a page that made an off-origin
    // link look first-party would be a phishing kit with a nice theme.
    assert!(
        page.body.contains("https://cosanlab.com/x"),
        "{}",
        page.body
    );
    assert!(
        page.body.contains("cosanlab.com<"),
        "the host is not shown: {}",
        page.body
    );
}

/// The toggle is the hangar's. A URL in somebody's email signature must not
/// break because its owner hid their index.
#[test]
fn a_switchboard_works_while_the_hangar_is_off() {
    let server = common::start();
    a_form(&server, "letter");
    assert_eq!(push(&server, "/v1/boards/hello", HELLO).status, 200);
    // No profile at all, so `enabled` is false.
    assert_eq!(get(&server, "/@alice").status, 404);
    assert_eq!(get(&server, "/@alice/hello").status, 200);
}

/// A line pointing at nothing is left off the page rather than served as a
/// dead button — and its owner is told in the cockpit instead.
#[test]
fn a_dark_line_is_omitted_from_the_page_and_reported_to_its_owner() {
    let server = common::start();
    // `letter` is never created, so the line has nothing to point at.
    assert_eq!(push(&server, "/v1/boards/hello", HELLO).status, 200);

    let page = get(&server, "/@alice/hello");
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        !page.body.contains("Request a letter"),
        "a dark line was rendered: {}",
        page.body
    );
    assert!(!page.body.contains("/f/alice/letter"), "{}", page.body);
    // The line that does resolve is still there.
    assert!(page.body.contains("Lab website"), "{}", page.body);

    // And the owner learns about it where they can act.
    let session = signed_in(&server);
    let cockpit = Request::new("GET", "/account", server.gate.to_string())
        .header("Cookie", &format!("__Host-factory-session={session}"))
        .send(server.gate);
    assert!(cockpit.body.contains("Dark lines"), "{}", cockpit.body);
    assert!(
        cockpit.body.contains("no form called `letter`"),
        "the reason is not named: {}",
        cockpit.body
    );
}

/// A form that exists but is not servable is a different fix from one that
/// does not exist, so the report says which.
#[test]
fn a_line_at_something_not_public_says_so_rather_than_saying_it_is_missing() {
    let server = common::start();
    // Parses, but declares no [verification], so it is never served.
    let manifest = "id = \"letter\"\nversion = 1\ntitle = \"Letter\"\n\
                    [[fields]]\nname = \"who\"\nlabel = \"You\"\nkind = \"text\"\nmax_length = 80\n";
    server
        .db
        .type_put(&mecha_factory::db::TypeRow {
            user_id: server.user.id.clone(),
            id: "letter".into(),
            title: "Letter".into(),
            manifest: manifest.into(),
            schema: "{}".into(),
            updated_at: mecha_factory::db::now(),
        })
        .unwrap();
    assert_eq!(push(&server, "/v1/boards/hello", HELLO).status, 200);

    let session = signed_in(&server);
    let cockpit = Request::new("GET", "/account", server.gate.to_string())
        .header("Cookie", &format!("__Host-factory-session={session}"))
        .send(server.gate);
    assert!(
        cockpit.body.contains("is not public"),
        "an unservable target read as missing: {}",
        cockpit.body
    );
}

/// A reserved slug answers what an unclaimed one answers, and neither can be
/// told from a handle that does not exist.
#[test]
fn a_reserved_or_unclaimed_slug_is_the_same_refusal() {
    let server = common::start();
    let unclaimed = get(&server, "/@alice/teaching");
    let reserved = get(&server, "/@alice/account");
    let no_person = get(&server, "/@nobody/teaching");
    assert_eq!(unclaimed.status, 404);
    assert_eq!(reserved.body, unclaimed.body);
    assert_eq!(no_person.body, unclaimed.body);
}

// ---- review fixes -------------------------------------------------------

/// A label is the author's own words and can say anything, so the host is
/// shown beside every link — including a labelled one, which is what the
/// masthead used to omit while the switchboard already showed it.
#[test]
fn a_labelled_profile_link_still_shows_where_it_goes() {
    let server = common::start();
    let profile = "enabled = true\ndisplay_name = \"Alice\"\n\
                   [[link]]\nlabel = \"Sign in to the factory\"\n\
                   url = \"https://evil.example/factory-login\"\n";
    assert_eq!(push(&server, "/v1/profile", profile).status, 200);

    let page = get(&server, "/@alice");
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body.contains("evil.example"),
        "a labelled off-origin link hid its destination: {}",
        page.body
    );
}

/// `https://trusted@evil.example/` is a legal URL a browser sends to
/// `evil.example`. Showing everything before the first `/` made the host
/// display an aid to the attack it exists to prevent.
#[test]
fn a_userinfo_url_does_not_borrow_a_trusted_hosts_name() {
    let server = common::start();
    let profile = format!(
        "enabled = true\n[[link]]\nurl = \"https://gate.{}@evil.example/signin\"\n",
        server.gate
    );
    assert_eq!(push(&server, "/v1/profile", &profile).status, 200);

    let page = get(&server, "/@alice");
    assert!(page.body.contains("evil.example"), "{}", page.body);
    assert!(
        !page
            .body
            .contains(&format!("gate.{}@evil.example<", server.gate)),
        "the userinfo was rendered as the host: {}",
        page.body
    );
}

/// `a` is a legal handle; only the *slug* `a` is reserved. A static segment
/// under `/@` shadowed every switchboard that user could ever make.
#[test]
fn a_one_letter_handle_can_still_serve_a_switchboard() {
    let server = common::start();
    let user = server.add_user("a");
    server
        .db
        .record_edit(
            &user.id,
            mecha_factory::db::RECORD_BOARD,
            "teaching",
            "slug = \"teaching\"\nheading = \"Teaching\"\n",
            &mecha_factory::db::now(),
        )
        .unwrap();

    let page = get(&server, "/@a/teaching");
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("Teaching"), "{}", page.body);

    // And the shared assets still serve, from outside the /@ namespace.
    let css = get(&server, "/a/form.css");
    assert_eq!(css.status, 200, "{}", css.head);
}

/// Nothing closes a poll when its deadline passes, so the inventory has to
/// apply the same predicate the handler does or the page advertises a ballot
/// that will refuse the answer.
#[test]
fn an_expired_poll_is_not_listed_even_though_its_state_is_open() {
    let server = common::start();
    assert_eq!(push(&server, "/v1/profile", ENABLED).status, 200);
    let spec = mecha_manifest::PollSpec::from_toml(
        "title = \"Lunch\"\n[audience]\nkind = \"link\"\nmax_ballots = 50\n\
         [[questions]]\nid = \"q\"\nkind = \"choice\"\n\
         [[questions.options]]\nid = \"a\"\nlabel = \"A\"\n\
         [[questions.options]]\nid = \"b\"\nlabel = \"B\"\n",
    )
    .unwrap();
    server
        .db
        .poll_create(
            &mecha_factory::db::PollRow {
                user_id: server.user.id.clone(),
                id: "lunch".into(),
                title: "Lunch poll".into(),
                timezone: "America/New_York".into(),
                duration_minutes: 0,
                deadline: Some("2020-01-01T00:00:00Z".into()),
                candidates: "[]".into(),
                spec: Some(serde_json::to_string(&spec).unwrap()),
                resolution: None,
                screen_token_hash: None,
                state: "open".into(),
                created_at: mecha_factory::db::now(),
                closed_at: None,
            },
            &[],
        )
        .unwrap();

    let page = get(&server, "/@alice");
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        !page.body.contains("/p/alice/lunch"),
        "an expired poll was advertised: {}",
        page.body
    );
}

/// A page whose bytes depend on a session must say so, or a shared cache may
/// hand one visitor another's chrome. There is no CDN in front today — which
/// is exactly the assumption that changes quietly, and `DEPLOY.md` says
/// adding one "changes nothing about the origin", true of routing and TLS
/// and false of this.
#[test]
fn every_session_dependent_page_refuses_a_shared_cache() {
    let server = common::start();
    assert_eq!(push(&server, "/v1/profile", ENABLED).status, 200);
    assert_eq!(
        push(&server, "/v1/boards/hello", "slug = \"hello\"\n").status,
        200
    );

    for path in ["/", "/account", "/@alice", "/@alice/hello"] {
        let reply = get(&server, path);
        let cache = reply.header("cache-control").unwrap_or_default();
        let vary = reply.header("vary").unwrap_or_default();
        assert!(
            cache.contains("private") && cache.contains("no-store"),
            "{path} may be cached by a proxy: cache-control={cache:?}"
        );
        assert!(
            vary.to_lowercase().contains("cookie"),
            "{path} varies on the session and does not say so: vary={vary:?}"
        );
    }
}

/// And a page that does *not* depend on a session should not claim to — the
/// header is a statement about the page, not decoration sprayed everywhere.
#[test]
fn a_stranger_only_page_does_not_claim_to_vary() {
    let server = common::start();
    let reply = get(&server, "/a/form.css");
    assert_eq!(reply.status, 200);
    assert!(
        !reply
            .header("vary")
            .unwrap_or_default()
            .to_lowercase()
            .contains("cookie"),
        "a shared asset claims to vary on the session"
    );
}
