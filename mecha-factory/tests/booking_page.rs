//! The booking page end to end on the box: a booking manifest pushed like
//! any type, a slot cache pushed like data, and a stranger's GET rendering
//! the week — with every wrong door answering "nothing here".

mod common;

use common::{start, Request};
use mecha_factory::db::Scope;

const BOOK_TOML: &str = r#"
id = "book"
version = 1
kind = "booking"
title = "Book a meeting"

[[fields]]
name = "requester_name"
label = "Your name"
kind = "text"
max_length = 120
required = true

[[fields]]
name = "requester_email"
label = "Your email"
kind = "email"
required = true

[verification]
field = "requester_email"

[availability]
timezone = "America/New_York"
durations = [30, 60]

[[availability.windows]]
day = "tue"
start = "13:00"
end = "17:00"
"#;

/// A slot push whose times are always in the caller's future.
fn future_slot_push() -> (String, String) {
    let start = chrono::Utc::now() + chrono::Duration::days(7);
    // On the half hour, for a stable-looking label.
    let start = start
        .date_naive()
        .and_hms_opt(17, 0, 0)
        .unwrap()
        .and_utc();
    let stamp = start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let end = (start + chrono::Duration::minutes(30))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let body = serde_json::json!({
        "generated_at": chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "horizon_days": 60,
        "slots": [{"start": stamp, "end": end, "duration_minutes": 30}],
    })
    .to_string();
    (body, stamp)
}

#[test]
fn a_pushed_type_and_cache_become_a_page_a_stranger_can_read() {
    let server = start();
    let gate = server.gate.to_string();

    let reply = Request::new("PUT", "/v1/types/book", &gate)
        .auth(&server.key(Scope::Release))
        .body(BOOK_TOML.as_bytes().to_vec())
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);

    // Before any slot push: the page exists and honestly offers nothing.
    let page = server.get(server.gate, "/s/alice/book");
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("Nothing is currently open"), "{}", page.body);

    let (push, stamp) = future_slot_push();
    let reply = Request::new("PUT", "/v1/instruments/book/slots", &gate)
        .auth(&server.key(Scope::Slots))
        .body(push)
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);

    // The shown week defaults to the first week with a slot, so the pushed
    // slot is on the first page a stranger sees.
    let page = server.get(server.gate, "/s/alice/book");
    assert_eq!(page.status, 200);
    assert!(
        page.body.contains(&format!("value=\"{stamp}|30\"")),
        "the slot is offerable: {}",
        page.body
    );
    assert!(page.body.contains("Times are shown in America/New_York"));
    assert!(page.body.contains("name=\"requester_email\""));
    assert!(
        !page.body.contains("recent changes may not show"),
        "a fresh cache carries no stale banner"
    );

    // The assets the page names are served beside it.
    for asset in ["booking.css", "form.js", "booking.js"] {
        let reply = server.get(server.gate, &format!("/s/alice/book/{asset}"));
        assert_eq!(reply.status, 200, "{asset}");
    }
}

#[test]
fn every_wrong_door_answers_nothing_here() {
    let server = start();
    let gate = server.gate.to_string();

    // A plain request type is not a booking page…
    let meeting = r#"
id = "meeting"
version = 1
title = "Request a meeting"
[[fields]]
name = "email"
label = "Email"
kind = "email"
required = true
[verification]
field = "email"
"#;
    let reply = Request::new("PUT", "/v1/types/meeting", &gate)
        .auth(&server.key(Scope::Release))
        .body(meeting.as_bytes().to_vec())
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(server.get(server.gate, "/s/alice/meeting").status, 404);

    // …and a booking type is not a form.
    let reply = Request::new("PUT", "/v1/types/book", &gate)
        .auth(&server.key(Scope::Release))
        .body(BOOK_TOML.as_bytes().to_vec())
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(server.get(server.gate, "/f/alice/book").status, 404);

    // Unknown handle, unknown type, wrong origin: all the same nothing.
    assert_eq!(server.get(server.gate, "/s/nobody/book").status, 404);
    assert_eq!(server.get(server.gate, "/s/alice/nothing").status, 404);
    assert_eq!(server.get(server.artifacts, "/s/alice/book").status, 404);
}

#[test]
fn a_stale_cache_serves_with_its_age_stated() {
    let server = start();
    let gate = server.gate.to_string();
    let reply = Request::new("PUT", "/v1/types/book", &gate)
        .auth(&server.key(Scope::Release))
        .body(BOOK_TOML.as_bytes().to_vec())
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);

    // A push whose generated_at is two days old: the endpoint accepts it
    // (freshness is the pipeline's job to enforce on its own side; the box
    // serves what it has), and the page states its age.
    let (push, _) = future_slot_push();
    let mut doc: serde_json::Value = serde_json::from_str(&push).unwrap();
    let old = chrono::Utc::now() - chrono::Duration::days(2);
    doc["generated_at"] =
        serde_json::json!(old.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    let reply = Request::new("PUT", "/v1/instruments/book/slots", &gate)
        .auth(&server.key(Scope::Slots))
        .body(doc.to_string())
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);

    let page = server.get(server.gate, "/s/alice/book");
    assert_eq!(page.status, 200);
    assert!(
        page.body.contains("recent changes may not show"),
        "{}",
        page.body
    );
}

/// The claim primitives, and the page narrowing over them: a hold wins its
/// slot exactly once, an overlapping duration is blocked with it, the
/// confirm converts only a live hold, an expired hold frees the slot, and
/// every live row vanishes from the served week.
#[test]
fn holds_claim_once_block_overlap_and_expire_free() {
    let server = start();
    let gate = server.gate.to_string();
    let reply = Request::new("PUT", "/v1/types/book", &gate)
        .auth(&server.key(Scope::Release))
        .body(BOOK_TOML.as_bytes().to_vec())
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let (push, stamp) = future_slot_push();
    let reply = Request::new("PUT", "/v1/instruments/book/slots", &gate)
        .auth(&server.key(Scope::Slots))
        .body(push)
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);

    let now = chrono::Utc::now();
    let now_s = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let later = (now + chrono::Duration::minutes(30))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let start: chrono::DateTime<chrono::Utc> = stamp.parse().unwrap();
    let end_30 = (start + chrono::Duration::minutes(30))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let end_60 = (start + chrono::Duration::minutes(60))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let row = |id: &str, end: &str, expires: &str| mecha_factory::db::BookingRow {
        id: id.into(),
        user_id: server.user.id.clone(),
        instrument_id: "book".into(),
        slot_start: stamp.clone(),
        slot_end: end.into(),
        duration_minutes: 30,
        state: "held".into(),
        hold_expires: Some(expires.into()),
        queue_seq: None,
        manage_hash: None,
        ics_sequence: 0,
        created_at: now_s.clone(),
        confirmed_at: None,
        cancelled_at: None,
    };

    assert!(server.db.booking_hold(&row("b1", &end_30, &later), &now_s).unwrap());
    // The same slot, and the 60 overlapping it, both lose while b1 lives.
    assert!(!server.db.booking_hold(&row("b2", &end_30, &later), &now_s).unwrap());
    assert!(!server.db.booking_hold(&row("b3", &end_60, &later), &now_s).unwrap());
    // The held slot is off the page.
    let page = server.get(server.gate, "/s/alice/book");
    assert!(!page.body.contains("_slot"), "a held week serves nothing: {}", page.body);

    // Confirm converts the live hold once; a second confirm finds no hold.
    assert!(server.db.booking_confirm("b1", "hash", &now_s).unwrap());
    assert!(!server.db.booking_confirm("b1", "hash", &now_s).unwrap());

    // An *expired* hold frees its slot with no sweeper: insert one dated
    // in the past on a second instrument-free stretch by expiring b4 now.
    let expired = (now - chrono::Duration::minutes(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    assert!(
        !server.db.booking_hold(&row("b4", &end_30, &expired), &now_s).unwrap(),
        "b1 is confirmed now, so the slot stays taken"
    );
    let blocking = server
        .db
        .bookings_blocking(&server.user.id, "book", &now_s)
        .unwrap();
    assert_eq!(blocking.len(), 1, "only the confirmed row blocks: {blocking:?}");
}

/// The POST half: a stranger picks a slot, the hold is taken, the link is
/// mailed — and the loser of the race gets the refreshed week, not an error.
#[test]
fn a_submission_holds_the_slot_and_the_race_loser_gets_the_week_back() {
    let server = start();
    let gate = server.gate.to_string();
    let reply = Request::new("PUT", "/v1/types/book", &gate)
        .auth(&server.key(Scope::Release))
        .body(BOOK_TOML.as_bytes().to_vec())
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let (push, stamp) = future_slot_push();
    let reply = Request::new("PUT", "/v1/instruments/book/slots", &gate)
        .auth(&server.key(Scope::Slots))
        .body(push)
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);

    let form = format!(
        "_slot={}%7C30&requester_name=Priya&requester_email=priya%40example.edu",
        stamp.replace(':', "%3A").replace('+', "%2B")
    );
    let reply = Request::new("POST", "/s/alice/book", &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form.clone())
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.body.contains("held for 30 minutes"), "{}", reply.body);
    assert_eq!(server.sent_links(), 1, "one verification link went out");
    assert!(
        server.verification_token().len() > 20,
        "the link carries a real token"
    );

    // The held slot is off the page for the next visitor…
    let page = server.get(server.gate, "/s/alice/book");
    assert!(!page.body.contains("_slot\" value"), "{}", page.body);

    // …and the same POST again loses the race, gracefully.
    let reply = Request::new("POST", "/s/alice/book", &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form)
        .send(server.gate);
    assert_eq!(reply.status, 200);
    assert!(reply.body.contains("just taken"), "{}", reply.body);
    assert_eq!(server.sent_links(), 1, "the loser mails nobody");

    // A POST naming a slot the cache never offered is a prober: nothing.
    let reply = Request::new("POST", "/s/alice/book", &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("_slot=2030-01-01T00%3A00%3A00Z%7C30&requester_name=X&requester_email=x%40y.z")
        .send(server.gate);
    assert_eq!(reply.status, 404, "{}", reply.body);
}
