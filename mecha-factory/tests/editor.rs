//! The cockpit's editor: writing a record in a browser.
//!
//! The rule the suite exists for is the second one in `http/editor.rs`: **a
//! rejected save never loses the text.** Everything else here is cheap to
//! re-test and that one is the difference between an editor somebody uses
//! twice and one they close.

mod common;

use common::{Reply, Request, Server};

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

fn get(server: &Server, target: &str, session: &str) -> Reply {
    Request::new("GET", target, server.gate.to_string())
        .header("Cookie", &format!("__Host-factory-session={session}"))
        .send(server.gate)
}

fn post(server: &Server, target: &str, body: &str, session: &str) -> Reply {
    Request::new("POST", target, server.gate.to_string())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Cookie", &format!("__Host-factory-session={session}"))
        .body(body.to_string())
        .send(server.gate)
}

fn csrf_of(server: &Server, session: &str) -> String {
    let page = get(server, "/account", session);
    let marker = "name=\"csrf\" value=\"";
    let start = page.body.find(marker).expect("a csrf token") + marker.len();
    page.body[start..].split('"').next().unwrap().to_string()
}

fn enc(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[test]
fn a_record_is_written_and_the_page_it_makes_is_served() {
    let server = common::start();
    let session = signed_in(&server);
    let csrf = csrf_of(&server, &session);

    let source = "enabled = true\ndisplay_name = \"Alice Chang\"\n";
    let reply = post(
        &server,
        "/account/edit",
        &format!("csrf={csrf}&what=profile&source={}", enc(source)),
        &session,
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.body.contains("Saved."), "{}", reply.body);

    // And the public page it enables is live, which is the whole point.
    let public = server.get(server.gate, "/@alice");
    assert_eq!(public.status, 200, "{}", public.body);
    assert!(public.body.contains("Alice Chang"), "{}", public.body);
}

/// The one that matters. Forty lines lost to a misplaced bracket is how an
/// in-place editor stops being used — and `mutating()` re-renders a *stale*
/// page, which is right for a button and wrong here.
#[test]
fn a_rejected_save_comes_back_with_the_text_still_in_the_box() {
    let server = common::start();
    let session = signed_in(&server);
    let csrf = csrf_of(&server, &session);

    // Valid TOML, invalid profile: `javascript:` is refused by the manifest.
    let typed = "enabled = true\ndisplay_name = \"Alice\"\n\
                 [[link]]\nurl = \"javascript:alert(1)\"\n";
    let reply = post(
        &server,
        "/account/edit",
        &format!("csrf={csrf}&what=profile&source={}", enc(typed)),
        &session,
    );
    assert_eq!(reply.status, 422, "{}", reply.body);
    assert!(reply.body.contains("Not saved."), "{}", reply.body);
    assert!(
        reply.body.contains("http(s)"),
        "no reason given: {}",
        reply.body
    );
    // Every line the person typed is still on the page, including the bad one.
    assert!(
        reply.body.contains("display_name = &quot;Alice&quot;")
            || reply.body.contains("display_name = \"Alice\""),
        "the typed text was lost: {}",
        reply.body
    );
    assert!(reply.body.contains("javascript:alert(1)"), "{}", reply.body);
    // And nothing was stored.
    assert_eq!(server.get(server.gate, "/@alice").status, 404);
}

/// Writing a board before the form it points at is an ordinary order to work
/// in, so the save lands and the page says which lines are dark.
#[test]
fn a_dark_line_warns_and_still_saves() {
    let server = common::start();
    let session = signed_in(&server);
    let csrf = csrf_of(&server, &session);

    assert_eq!(
        post(
            &server,
            "/account/boards",
            &format!("csrf={csrf}&slug=hello&confirm=yes"),
            &session
        )
        .status,
        303
    );
    let source = "slug = \"hello\"\nheading = \"Get in touch\"\n\
                  [[entry]]\nkind = \"form\"\nid = \"letter\"\nlabel = \"Write\"\n";
    let reply = post(
        &server,
        "/account/edit",
        &format!("csrf={csrf}&what=board&slug=hello&source={}", enc(source)),
        &session,
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.body.contains("Saved."), "{}", reply.body);
    assert!(reply.body.contains("point at nothing"), "{}", reply.body);
    assert!(reply.body.contains("no form called"), "{}", reply.body);

    // The page exists and simply carries no lines.
    let public = server.get(server.gate, "/@alice/hello");
    assert_eq!(public.status, 200, "{}", public.body);
    assert!(!public.body.contains("Write"), "{}", public.body);
}

/// A name here can never be reissued, so it must not be spent as a side
/// effect of typing one.
#[test]
fn creating_a_page_requires_confirming_the_name_is_permanent() {
    let server = common::start();
    let session = signed_in(&server);
    let csrf = csrf_of(&server, &session);

    let reply = post(
        &server,
        "/account/boards",
        &format!("csrf={csrf}&slug=teaching"),
        &session,
    );
    assert_eq!(reply.status, 422, "{}", reply.body);
    assert!(reply.body.contains("permanent"), "{}", reply.body);
    assert_eq!(server.get(server.gate, "/@alice/teaching").status, 404);

    // Reserved names and repeats are refused too.
    for (slug, why) in [("account", "reserved"), ("teaching", "already")] {
        if why == "already" {
            assert_eq!(
                post(
                    &server,
                    "/account/boards",
                    &format!("csrf={csrf}&slug=teaching&confirm=yes"),
                    &session
                )
                .status,
                303
            );
        }
        let reply = post(
            &server,
            "/account/boards",
            &format!("csrf={csrf}&slug={slug}&confirm=yes"),
            &session,
        );
        assert_eq!(reply.status, 422, "{slug}: {}", reply.body);
        assert!(reply.body.contains(why), "{slug}: {}", reply.body);
    }
}

/// The editor is a signed-in surface and nothing else.
#[test]
fn the_editor_needs_a_session_and_a_csrf_token() {
    let server = common::start();
    let anonymous = server.get(server.gate, "/account/edit/profile");
    assert!(anonymous.body.contains("Sign in"), "{}", anonymous.body);

    let session = signed_in(&server);
    let reply = post(
        &server,
        "/account/edit",
        "csrf=wrong&what=profile&source=enabled+%3D+true",
        &session,
    );
    assert_eq!(reply.status, 403, "{}", reply.body);
    assert_eq!(server.get(server.gate, "/@alice").status, 404);
}

/// A cockpit edit leaves `baseline` alone, which is what lets the next push
/// tell that this field was changed here rather than flattening it.
#[test]
fn an_edit_here_is_visible_to_the_next_push_as_drift() {
    let server = common::start();
    let session = signed_in(&server);
    let csrf = csrf_of(&server, &session);

    // Pushed from a machine first, so there is a baseline to drift from.
    Request::new("PUT", "/v1/profile", server.gate.to_string())
        .auth(&server.key(mecha_factory::db::Scope::Release))
        .body("enabled = true\ntagline = \"From the file\"\n".to_string())
        .send(server.gate);

    post(
        &server,
        "/account/edit",
        &format!(
            "csrf={csrf}&what=profile&source={}",
            enc("enabled = true\ntagline = \"From the browser\"\n")
        ),
        &session,
    );

    let row = server
        .db
        .record_get(&server.user.id, mecha_factory::db::RECORD_PROFILE, "")
        .unwrap()
        .unwrap();
    assert!(row.drifted(), "the edit is invisible to a later push");
    assert!(row.baseline.contains("From the file"));
    assert!(row.effective.contains("From the browser"));
}
