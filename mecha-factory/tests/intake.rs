//! A stranger, a form, and a link — the state machine from outside.
//!
//! The property every test here is really about: **nothing in this path needs
//! an agent.** The origin renders from a manifest, validates against a schema,
//! and answers with a confirmation, all uploaded earlier. A detached instrument
//! degrades from responsive to collecting, and these are the assertions that
//! say so.

mod common;

use common::{start, Request, Server};
use mecha_factory::db::Scope;

fn upload_meeting(server: &Server) {
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../mecha-manifest/types/meeting.toml"),
    )
    .unwrap();
    let reply = Request::new("PUT", "/v1/types/meeting", server.gate.to_string())
        .auth(&server.key(Scope::Release))
        .body(manifest)
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
}

fn submit(server: &Server, body: &str) -> common::Reply {
    Request::new("POST", "/f/alice/meeting", server.gate.to_string())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send(server.gate)
}

/// A submission that validates. `purpose_detail` is deliberately absent:
/// it is `show_when` purpose = "other", and a field the browser did not show
/// is one the server refuses — which is the rule that makes conditional forms
/// safe rather than a second, weaker schema.
const GOOD: &str = "requester_name=Ada+Lovelace&requester_email=ada%40example.org\
&affiliation=Analytical+Engines&purpose=collaboration\
&duration_minutes=30&understands_request=on";

/// The whole path: form, submission, link, confirmation, and only then a
/// record home can see.
#[test]
fn a_stranger_can_fill_a_form_and_nothing_reaches_the_queue_until_they_click() {
    let server = start();
    upload_meeting(&server);

    // The form renders from the manifest, with no agent anywhere.
    let form = server.get(server.gate, "/f/alice/meeting");
    assert_eq!(form.status, 200);
    assert!(form.body.contains("requester_email"), "{}", form.body);
    assert!(form.body.contains("<form"), "{}", form.body);

    let reply = submit(&server, GOOD);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.body.contains("Check your email"), "{}", reply.body);

    // Submitted is not queued: an unverified row costs a little disk and never
    // a triage run.
    let drained = Request::new("GET", "/v1/queue", server.gate.to_string())
        .auth(&server.key(Scope::Drain))
        .send(server.gate);
    assert!(
        drained.json()["records"].as_array().unwrap().is_empty(),
        "{}",
        drained.body
    );

    // The link, which in this build goes to the log rather than to an inbox.
    let token = server.verification_token();
    let confirm = server.get(server.gate, &format!("/f/alice/meeting/c/{token}"));
    assert_eq!(confirm.status, 200, "{}", confirm.body);
    assert!(
        confirm.body.contains("that&#39;s in the queue"),
        "{}",
        confirm.body
    );
    // The confirmation interpolates their own answer back to them.
    assert!(
        confirm.body.contains("A possible collaboration") || confirm.body.contains("collaboration"),
        "{}",
        confirm.body
    );

    // And now home can see it.
    let drained = Request::new("GET", "/v1/queue", server.gate.to_string())
        .auth(&server.key(Scope::Drain))
        .send(server.gate);
    let records = drained.json();
    assert_eq!(records["records"].as_array().unwrap().len(), 1);
    assert_eq!(records["records"][0]["type"], "meeting");
    assert!(
        records["records"][0]["payload"]
            .as_str()
            .unwrap()
            .contains("ada@example.org"),
        "{}",
        drained.body
    );
}

/// A link works once, and the page says the same thing for used, expired and
/// never-existed — because which of the three it was is not a stranger's
/// business, and "already used" would tell whoever forwarded it that somebody
/// clicked.
#[test]
fn a_confirmation_link_is_single_use() {
    let server = start();
    upload_meeting(&server);
    submit(&server, GOOD);
    let token = server.verification_token();

    assert_eq!(
        server
            .get(server.gate, &format!("/f/alice/meeting/c/{token}"))
            .status,
        200
    );

    let again = server.get(server.gate, &format!("/f/alice/meeting/c/{token}"));
    assert_eq!(again.status, 404);
    assert!(again.body.contains("expired"), "{}", again.body);

    let invented = server.get(server.gate, "/f/alice/meeting/c/0123456789abcdef");
    assert_eq!(invented.status, 404);
    assert_eq!(
        invented.body, again.body,
        "a used link and an invented one have to read identically"
    );
}

/// Their own form back, with the errors on it — and nothing stored.
#[test]
fn an_invalid_submission_is_returned_not_stored() {
    let server = start();
    upload_meeting(&server);

    let reply = submit(
        &server,
        "requester_name=Ada&requester_email=not-an-address&purpose=collaboration",
    );
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(reply.body.contains("<form"), "the form comes back");
    assert!(
        reply.body.contains("Ada"),
        "with what they typed still in it"
    );
    assert_eq!(server.db.queue_depth(None).unwrap(), 0);
}

/// Forty mails to one person is abuse; forty people once may be a conference.
#[test]
fn verification_sends_are_budgeted_per_recipient_and_per_user() {
    let server = start();
    upload_meeting(&server);

    for i in 0..3 {
        assert_eq!(submit(&server, GOOD).status, 200, "submission {i}");
    }
    let refused = submit(&server, GOOD);
    assert_eq!(refused.status, 429, "{}", refused.body);

    // A different address is a different budget…
    let other = GOOD.replace("ada%40example.org", "bob%40example.org");
    assert_eq!(submit(&server, &other).status, 200);

    // …until the user's own daily budget runs out.
    // The user's own daily budget is a column; drop it below what has already
    // been sent today.
    server.db.user_send_budget(&server.user.id, 1).unwrap();
    let refused = submit(&server, &GOOD.replace("ada%40", "carol%40"));
    assert_eq!(refused.status, 429, "{}", refused.body);
}

/// An abandoned submission is a stranger's data with no consent behind it, and
/// "never verified" is the one state where keeping the record serves nobody.
#[test]
fn an_unverified_submission_expires_and_is_deleted() {
    let server = start();
    upload_meeting(&server);
    submit(&server, GOOD);
    assert_eq!(server.db.queue_depth(None).unwrap(), 0, "not queued");

    // Nothing expires yet.
    assert_eq!(
        server.db.expire_unverified("2026-08-06T00:00:00Z").unwrap().0,
        0
    );

    // Well past the 48-hour window.
    assert_eq!(
        server.db.expire_unverified("2030-01-01T00:00:00Z").unwrap().0,
        1
    );
    let token = server.verification_token();
    assert_eq!(
        server
            .get(server.gate, &format!("/f/alice/meeting/c/{token}"))
            .status,
        404,
        "a link to a swept row confirms nothing"
    );
}

/// A stranger learns the same thing from every kind of absence: that there is
/// no form here.
#[test]
fn nothing_distinguishes_the_ways_a_form_can_be_missing() {
    let server = start();
    upload_meeting(&server);
    let real = server.get(server.gate, "/f/alice/meeting");
    assert_eq!(real.status, 200);

    let cases = [
        "/f/alice/nosuchtype", // a type she does not have
        "/f/mallory/meeting",  // a user who does not exist
        "/f/bob/meeting",      // a user who exists, later
    ];
    server.add_user("bob");
    let mut bodies = Vec::new();
    for target in cases {
        let reply = server.get(server.gate, target);
        assert_eq!(reply.status, 404, "{target}");
        bodies.push(reply.body);
    }
    assert!(
        bodies.windows(2).all(|w| w[0] == w[1]),
        "the absences read differently: {bodies:?}"
    );

    // And a suspended user's form stops existing too.
    server.db.user_status(&server.user.id, "suspended").unwrap();
    let reply = server.get(server.gate, "/f/alice/meeting");
    assert_eq!(reply.status, 404);
    assert_eq!(reply.body, bodies[0]);
}

/// A type with no `[verification]` block cannot be served at all: an
/// unverified submission would cost a triage run for an address nobody proved.
#[test]
fn a_type_without_verification_is_not_served_as_a_form() {
    let server = start();
    let toml = r#"
id = "unverified"
version = 1
title = "No verification"
[[fields]]
name = "note"
label = "A note"
kind = "text"
max_length = 100
"#;
    let reply = Request::new("PUT", "/v1/types/unverified", server.gate.to_string())
        .auth(&server.key(Scope::Release))
        .body(toml)
        .send(server.gate);
    assert_eq!(reply.status, 200, "uploading it is fine: {}", reply.body);

    // Serving it is not.
    assert_eq!(server.get(server.gate, "/f/alice/unverified").status, 404);
    assert_eq!(
        Request::new("POST", "/f/alice/unverified", server.gate.to_string())
            .body("note=hello")
            .send(server.gate)
            .status,
        404
    );
}

// ---- the upload step ------------------------------------------------------

/// A letter-ish type with one required file field, servable.
fn upload_letterish(server: &Server) {
    let toml = r#"
id = "letterish"
version = 1
title = "Request a letter"

[[fields]]
name = "requester_email"
label = "Your email"
kind = "email"
required = true

[[fields]]
name = "cv"
label = "Your CV"
kind = "file"
accept = ["pdf"]
max_bytes = 1048576
required = true

[verification]
field = "requester_email"
"#;
    let reply = Request::new("PUT", "/v1/types/letterish", server.gate.to_string())
        .auth(&server.key(Scope::Release))
        .body(toml)
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
}

fn multipart(boundary: &str, parts: &[(&str, &str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, filename, bytes) in parts {
        out.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; \
                 name=\"{name}\"; filename=\"{filename}\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        out.extend_from_slice(bytes);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

fn post_upload(server: &Server, location: &str, parts: &[(&str, &str, &[u8])]) -> common::Reply {
    let body = multipart("testboundary", parts);
    Request::new("POST", location, server.gate.to_string())
        .header(
            "Content-Type",
            "multipart/form-data; boundary=testboundary",
        )
        .body(body)
        .send(server.gate)
}

/// The whole verified-only upload arc: the public form carries no file input,
/// the click redirects to a reloadable upload page, the sniffed bytes decide,
/// and only the completed row reaches the queue — with its blob in the ledger
/// and on disk.
#[test]
fn a_file_arrives_only_after_verification_and_only_as_what_it_is() {
    let server = start();
    upload_letterish(&server);

    // The public form: a note where the input would be, and no enctype.
    let form = server.get(server.gate, "/f/alice/letterish");
    assert_eq!(form.status, 200);
    assert!(form.body.contains("after verifying your email"), "{}", form.body);
    assert!(!form.body.contains("type=\"file\""), "{}", form.body);

    // A urlencoded value aimed at the file field is a probe, refused.
    let probe = Request::new("POST", "/f/alice/letterish", server.gate.to_string())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("requester_email=ada%40example.org&cv=hello")
        .send(server.gate);
    assert_eq!(probe.status, 400, "{}", probe.body);

    // Submit without the file: fine at this phase.
    let reply = Request::new("POST", "/f/alice/letterish", server.gate.to_string())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("requester_email=ada%40example.org")
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);

    // The click: not queued yet — redirected to the upload page.
    let token = server.verification_token();
    let confirm = server.get(server.gate, &format!("/f/alice/letterish/c/{token}"));
    assert_eq!(confirm.status, 303, "{}", confirm.body);
    let location = confirm.header("location").expect("a redirect names where");
    assert!(location.contains("/u/"), "{location}");
    assert_eq!(server.db.queue_depth(None).unwrap(), 0, "not yet queued");

    // The upload page is a pure read: reloadable, with a real file input.
    for _ in 0..2 {
        let page = server.get(server.gate, &location);
        assert_eq!(page.status, 200, "{}", page.body);
        assert!(page.body.contains("type=\"file\""), "{}", page.body);
        assert!(page.body.contains("multipart/form-data"), "{}", page.body);
    }

    // A PNG pretending to be a PDF: the magic decides, the field refuses,
    // nothing is written, and the token survives to try again.
    let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
    let spoofed = post_upload(&server, &location, &[("cv", "cv.pdf", &png)]);
    assert_eq!(spoofed.status, 400, "{}", spoofed.body);
    assert!(spoofed.body.contains("takes: pdf"), "{}", spoofed.body);
    assert_eq!(server.db.queue_depth(None).unwrap(), 0);
    assert_eq!(server.get(server.gate, &location).status, 200, "still live");

    // The real thing.
    let done = post_upload(&server, &location, &[("cv", "Ada CV.pdf", b"%PDF-1.4 tiny")]);
    assert_eq!(done.status, 200, "{}", done.body);
    assert_eq!(server.db.queue_depth(None).unwrap(), 1);

    // The row's payload carries the measurements and not the bytes; the
    // ledger carries the blob; the blob is on disk and matches its digest.
    let drained = Request::new("GET", "/v1/queue", server.gate.to_string())
        .auth(&server.key(Scope::Drain))
        .send(server.gate);
    let records = drained.json();
    let payload = records["records"][0]["payload"].as_str().unwrap();
    assert!(payload.contains("sha256:"), "{payload}");
    assert!(payload.contains("application/pdf"), "{payload}");
    let seq = records["records"][0]["seq"].as_i64().unwrap();

    let user = server.db.user_by_handle("alice").unwrap().unwrap();
    let rows = server.db.attachments_for(&user.id, seq).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].filename, "Ada CV.pdf");
    assert_eq!(rows[0].content_type, "application/pdf");
    let blob = server.dir.path().join("attachments").join(&user.id).join(&rows[0].id);
    assert_eq!(std::fs::read(&blob).unwrap(), b"%PDF-1.4 tiny");

    // The token spent with the upload: the page is gone now.
    assert_eq!(server.get(server.gate, &location).status, 404);

    // The blob route: drain-scoped, a pure read, and one 404 for someone
    // else's id and a nonexistent one alike.
    let blob_path = format!("/v1/queue/attachments/{}", rows[0].id);
    for _ in 0..2 {
        let fetched = Request::new("GET", &blob_path, server.gate.to_string())
            .auth(&server.key(Scope::Drain))
            .send(server.gate);
        assert_eq!(fetched.status, 200);
        assert_eq!(fetched.body.as_bytes(), b"%PDF-1.4 tiny");
    }
    let wrong_scope = Request::new("GET", &blob_path, server.gate.to_string())
        .auth(&server.key(Scope::Publish))
        .send(server.gate);
    assert_ne!(wrong_scope.status, 200, "a publish key reads no queue blobs");
    let invented = Request::new("GET", "/v1/queue/attachments/00000000", server.gate.to_string())
        .auth(&server.key(Scope::Drain))
        .send(server.gate);
    assert_eq!(invented.status, 404);

    // And the drain names the blob beside the record it belongs to.
    let drained = Request::new("GET", "/v1/queue", server.gate.to_string())
        .auth(&server.key(Scope::Drain))
        .send(server.gate);
    let atts = &drained.json()["records"][0]["attachments"];
    assert_eq!(atts[0]["id"], rows[0].id.as_str());
    assert_eq!(atts[0]["field"], "cv");
    assert_eq!(atts[0]["filename"], "Ada CV.pdf");
    assert_eq!(atts[0]["content_type"], "application/pdf");

    // And the ack takes the blob with the row.
    let ack = Request::new("POST", "/v1/queue/ack", server.gate.to_string())
        .auth(&server.key(Scope::Drain))
        .body(format!(r#"{{"seqs":[{seq}]}}"#))
        .send(server.gate);
    assert_eq!(ack.status, 200, "{}", ack.body);
    assert!(!blob.exists(), "an acknowledged row's file goes with it");
}

/// An optional attachment is declined by submitting nothing — absence is the
/// decline, so there is no skip verb to drift from the validator.
#[test]
fn an_optional_file_can_be_declined_by_uploading_nothing() {
    let server = start();
    let toml = r#"
id = "optionalish"
version = 1
title = "Optional attachment"

[[fields]]
name = "requester_email"
label = "Your email"
kind = "email"
required = true

[[fields]]
name = "materials"
label = "Anything useful"
kind = "file"
accept = ["pdf", "png"]

[verification]
field = "requester_email"
"#;
    let reply = Request::new("PUT", "/v1/types/optionalish", server.gate.to_string())
        .auth(&server.key(Scope::Release))
        .body(toml)
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);

    Request::new("POST", "/f/alice/optionalish", server.gate.to_string())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("requester_email=ada%40example.org")
        .send(server.gate);
    let token = server.verification_token();
    let confirm = server.get(server.gate, &format!("/f/alice/optionalish/c/{token}"));
    assert_eq!(confirm.status, 303);
    let location = confirm.header("location").unwrap();

    // The browser sends an empty part for an untouched file input.
    let done = post_upload(&server, &location, &[("materials", "", b"")]);
    assert_eq!(done.status, 200, "{}", done.body);
    assert_eq!(server.db.queue_depth(None).unwrap(), 1);
    let user = server.db.user_by_handle("alice").unwrap().unwrap();
    let drained = Request::new("GET", "/v1/queue", server.gate.to_string())
        .auth(&server.key(Scope::Drain))
        .send(server.gate);
    let seq = drained.json()["records"][0]["seq"].as_i64().unwrap();
    assert!(server.db.attachments_for(&user.id, seq).unwrap().is_empty());
}
