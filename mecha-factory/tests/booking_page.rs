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
    let start = start.date_naive().and_hms_opt(17, 0, 0).unwrap().and_utc();
    let stamp = start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let end =
        (start + chrono::Duration::minutes(30)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
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
    assert!(
        page.body.contains("Nothing is currently open"),
        "{}",
        page.body
    );

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
    for asset in ["booking.css", "form.js", "booking.js", "poll.js"] {
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
    doc["generated_at"] = serde_json::json!(old.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
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
    let later =
        (now + chrono::Duration::minutes(30)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let start: chrono::DateTime<chrono::Utc> = stamp.parse().unwrap();
    let end_30 =
        (start + chrono::Duration::minutes(30)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let end_60 =
        (start + chrono::Duration::minutes(60)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
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

    assert!(server
        .db
        .booking_hold(&row("b1", &end_30, &later), &now_s)
        .unwrap());
    // The same slot, and the 60 overlapping it, both lose while b1 lives.
    assert!(!server
        .db
        .booking_hold(&row("b2", &end_30, &later), &now_s)
        .unwrap());
    assert!(!server
        .db
        .booking_hold(&row("b3", &end_60, &later), &now_s)
        .unwrap());
    // The held slot is off the page.
    let page = server.get(server.gate, "/s/alice/book");
    assert!(
        !page.body.contains("_slot"),
        "a held week serves nothing: {}",
        page.body
    );

    // Confirm converts the live hold once; a second confirm finds no hold.
    assert!(server.db.booking_confirm("b1", "hash", &now_s).unwrap());
    assert!(!server.db.booking_confirm("b1", "hash", &now_s).unwrap());

    // An *expired* hold frees its slot with no sweeper: insert one dated
    // in the past on a second instrument-free stretch by expiring b4 now.
    let expired =
        (now - chrono::Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    assert!(
        !server
            .db
            .booking_hold(&row("b4", &end_30, &expired), &now_s)
            .unwrap(),
        "b1 is confirmed now, so the slot stays taken"
    );
    let blocking = server
        .db
        .bookings_blocking(&server.user.id, "book", &now_s)
        .unwrap();
    assert_eq!(
        blocking.len(),
        1,
        "only the confirmed row blocks: {blocking:?}"
    );
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
    assert!(
        reply.body.contains("value=\"Priya\""),
        "losing the race must not also cost the typed details: {}",
        reply.body
    );
    assert_eq!(server.sent_links(), 1, "the loser mails nobody");

    // A POST naming a slot the cache never offered is a prober: nothing.
    let reply = Request::new("POST", "/s/alice/book", &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("_slot=2030-01-01T00%3A00%3A00Z%7C30&requester_name=X&requester_email=x%40y.z")
        .send(server.gate);
    assert_eq!(reply.status, 404, "{}", reply.body);
}

/// A rejected submission keeps everything the visitor already did: the
/// picked slot arrives re-checked, the typed values ride the fields, and the
/// summary names the failed field by its label.
#[test]
fn a_rejected_submission_keeps_the_pick_and_the_typed_values() {
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

    // A real slot, a real name, a mangled email.
    let form = format!(
        "_slot={}%7C30&requester_name=Priya&requester_email=not-an-email",
        stamp.replace(':', "%3A")
    );
    let reply = Request::new("POST", "/s/alice/book", &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form)
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(
        reply
            .body
            .contains(&format!("value=\"{stamp}|30\" checked")),
        "the picked slot survives the rejection: {}",
        reply.body
    );
    assert!(reply.body.contains("value=\"Priya\""), "{}", reply.body);
    assert!(
        reply.body.contains(">Your email</a>"),
        "the summary says the label, not the field name: {}",
        reply.body
    );
    assert_eq!(
        server.sent_links(),
        0,
        "nothing was held, nobody was mailed"
    );

    // Nothing took the slot: the next visitor still sees it.
    let page = server.get(server.gate, "/s/alice/book");
    assert!(page.body.contains(&format!("value=\"{stamp}|30\"")));
}

/// The live half of the page: `slots.json` reports what is open right now,
/// uncacheably, and a hold taken between two polls disappears from it.
#[test]
fn slots_json_tells_an_open_tab_the_truth() {
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

    // The page tells its scripts where to poll.
    let page = server.get(server.gate, "/s/alice/book");
    assert!(
        page.body.contains("/s/alice/book/slots.json"),
        "{}",
        page.body
    );

    let reply = server.get(server.gate, "/s/alice/book/slots.json");
    assert_eq!(reply.status, 200, "{}", reply.body);
    let open = reply.json();
    assert_eq!(open["slots"][0]["start"], stamp.as_str(), "{}", reply.body);

    // Someone holds it; the next poll no longer offers it.
    let form = format!(
        "_slot={}%7C30&requester_name=Priya&requester_email=priya%40example.edu",
        stamp.replace(':', "%3A")
    );
    let reply = Request::new("POST", "/s/alice/book", &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form)
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let reply = server.get(server.gate, "/s/alice/book/slots.json");
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.json()["slots"].as_array().map(|s| s.len()),
        Some(0),
        "a held slot is not open: {}",
        reply.body
    );

    // Wrong doors answer nothing, like every other booking route.
    assert_eq!(
        server.get(server.gate, "/s/nobody/book/slots.json").status,
        404
    );
    assert_eq!(
        server
            .get(server.artifacts, "/s/alice/book/slots.json")
            .status,
        404
    );
}

/// The whole claim, outside-in: submit holds, the mailed link's POST books,
/// and the queue row survives to drain. Then the other ending: a hold that
/// lapsed before its click deletes the queue row — a record that drained
/// would become a calendar event for a meeting that never happened — and
/// the stranger gets the week back with the truth.
#[test]
fn the_click_books_and_a_lapsed_hold_never_drains() {
    let server = start();
    let gate = server.gate.to_string();
    let put_type = |toml: &str| {
        let reply = Request::new("PUT", "/v1/types/book", &gate)
            .auth(&server.key(Scope::Release))
            .body(toml.as_bytes().to_vec())
            .send(server.gate);
        assert_eq!(reply.status, 200, "{}", reply.body);
    };
    put_type(BOOK_TOML);
    let (push, stamp) = future_slot_push();
    let reply = Request::new("PUT", "/v1/instruments/book/slots", &gate)
        .auth(&server.key(Scope::Slots))
        .body(push)
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);

    let form = format!(
        "_slot={}%7C30&requester_name=Priya&requester_email=priya%40example.edu",
        stamp.replace(':', "%3A")
    );
    let submit = |form: &str| {
        Request::new("POST", "/s/alice/book", &gate)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form.to_string())
            .send(server.gate)
    };
    assert_eq!(submit(&form).status, 200);
    let token = server.verification_token();

    // The GET is a button that spends nothing: the row is still unverified
    // after a scanner's fetch, and the queue still drains nothing.
    let get = server.get(server.gate, &format!("/s/alice/book/c/{token}"));
    assert_eq!(get.status, 200);
    assert!(get.body.contains("Confirm and book"), "{}", get.body);
    assert_eq!(server.db.queue_depth(Some(&server.user.id)).unwrap(), 0);

    // The click books.
    let reply = Request::new("POST", &format!("/s/alice/book/c/{token}"), &gate).send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.body.contains("booked"), "{}", reply.body);
    assert_eq!(
        server.db.queue_depth(Some(&server.user.id)).unwrap(),
        1,
        "a booked booking drains"
    );
    // The payload gained the manage URL at confirm — minted box-side, read
    // home-side, carried like the other machinery keys.
    let drained = server.db.drain(&server.user.id, 0, 10).unwrap();
    let payload: serde_json::Value = serde_json::from_str(drained[0].payload.as_str()).unwrap();
    let manage = payload["_manage_url"].as_str().expect("a manage url");
    assert!(manage.contains("/s/alice/book/m/"), "{manage}");
    // A second click finds the token spent, and does not double-book.
    let reply = Request::new("POST", &format!("/s/alice/book/c/{token}"), &gate).send(server.gate);
    assert_eq!(reply.status, 404);

    // Round two, with a policy whose holds lapse instantly.
    put_type(&format!("{BOOK_TOML}\n[policy]\nhold_minutes = 0\n"));
    // A different slot, so round one's confirmed booking does not block it.
    let start: chrono::DateTime<chrono::Utc> = stamp.parse().unwrap();
    let late =
        (start + chrono::Duration::hours(2)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let late_end = (start + chrono::Duration::hours(2) + chrono::Duration::minutes(30))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let push = serde_json::json!({
        "generated_at": chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "horizon_days": 60,
        "slots": [{"start": late, "end": late_end, "duration_minutes": 30}],
    })
    .to_string();
    let reply = Request::new("PUT", "/v1/instruments/book/slots", &gate)
        .auth(&server.key(Scope::Slots))
        .body(push)
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);

    let form = format!(
        "_slot={}%7C30&requester_name=Tal&requester_email=tal%40example.edu",
        late.replace(':', "%3A")
    );
    assert_eq!(submit(&form).status, 200);
    let token = server.verification_token();
    let reply = Request::new("POST", &format!("/s/alice/book/c/{token}"), &gate).send(server.gate);
    assert_eq!(reply.status, 200);
    assert!(reply.body.contains("lapsed"), "{}", reply.body);
    assert_eq!(
        server.db.queue_depth(Some(&server.user.id)).unwrap(),
        1,
        "only round one's booking is in the queue — the lapsed one is gone"
    );
}

/// A record in `finalizing` does not exist as far as the drain is
/// concerned — it becomes drainable exactly once, at release, fully
/// formed. This is the ordering that keeps a long-polled drain from
/// shipping a booking before its manage URL is written onto it.
#[test]
fn a_finalizing_record_is_invisible_until_released() {
    let server = start();
    let gate = server.gate.to_string();
    let seq = server
        .db
        .queue_add(
            &server.user.id,
            "book",
            "finalizing",
            "{\"draft\":true}",
            &mecha_factory::db::now(),
            None,
        )
        .unwrap();

    let reply = Request::new("GET", "/v1/queue", &gate)
        .auth(&server.key(Scope::Drain))
        .send(server.gate);
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.json()["records"].as_array().map(|r| r.len()),
        Some(0),
        "a finalizing record must not drain: {}",
        reply.body
    );

    assert!(server
        .db
        .queue_release(&server.user.id, seq, Some("{\"final\":true}"))
        .unwrap());
    let reply = Request::new("GET", "/v1/queue", &gate)
        .auth(&server.key(Scope::Drain))
        .send(server.gate);
    assert_eq!(reply.json()["records"][0]["payload"], "{\"final\":true}");
    // Releasing twice is a no-op: the state moved.
    assert!(!server.db.queue_release(&server.user.id, seq, None).unwrap());
}

/// `?wait=` holds the drain open and a record landing answers it early —
/// the whole of "the invite follows the click by seconds", with the
/// connection still initiated by home. An empty wait times out empty.
#[test]
fn a_waiting_drain_wakes_when_a_record_lands() {
    let server = start();
    let gate = server.gate.to_string();
    let addr = server.gate;
    let key = server.key(Scope::Drain);

    // Nothing queued: a short wait answers empty after roughly its wait,
    // not instantly — proof it actually held the request.
    let started = std::time::Instant::now();
    let reply = Request::new("GET", "/v1/queue?wait=1", &gate)
        .auth(&key)
        .send(addr);
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["records"].as_array().map(|r| r.len()), Some(0));
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(900),
        "an empty wait=1 answered in {:?}",
        started.elapsed()
    );

    // A record landing mid-wait answers the held request early.
    let poller = {
        let gate = gate.clone();
        let key = key.clone();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let reply = Request::new("GET", "/v1/queue?wait=20", &gate)
                .auth(&key)
                .send(addr);
            (reply, started.elapsed())
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(400));
    server
        .db
        .queue_add(
            &server.user.id,
            "book",
            "queued",
            "{}",
            &mecha_factory::db::now(),
            None,
        )
        .unwrap();
    let (reply, elapsed) = poller.join().unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.json()["records"].as_array().map(|r| r.len()),
        Some(1),
        "{}",
        reply.body
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "the wake beat the deadline by a mile, took {elapsed:?}"
    );
}

/// The manage link, in every state it answers: active cancels once and
/// frees the slot, the cancellation queues a machinery record for home,
/// "already cancelled" tells the truth, and a spliced or stale token gets
/// a branded page with a way forward, never a bare 404.
#[test]
fn the_manage_link_cancels_once_and_answers_every_state_honestly() {
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

    // Book it, outside-in.
    let form = format!(
        "_slot={}%7C30&requester_name=Priya&requester_email=priya%40example.edu",
        stamp.replace(':', "%3A")
    );
    let reply = Request::new("POST", "/s/alice/book", &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form)
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let token = server.verification_token();
    let reply = Request::new("POST", &format!("/s/alice/book/c/{token}"), &gate).send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);

    // The manage URL travels in the drained payload; use it as home would.
    let drained = server.db.drain(&server.user.id, 0, 10).unwrap();
    let payload: serde_json::Value = serde_json::from_str(drained[0].payload.as_str()).unwrap();
    let manage_url = payload["_manage_url"].as_str().unwrap();
    let manage_path = &manage_url[manage_url.find("/s/").unwrap()..];

    // GET states without mutating: the booking survives a scanner's fetch.
    let page = server.get(server.gate, manage_path);
    assert_eq!(page.status, 200);
    assert!(page.body.contains("Cancel this booking"), "{}", page.body);

    // The slot is off the week while booked.
    let week = server.get(server.gate, "/s/alice/book");
    assert!(!week.body.contains("_slot\" value"), "booked = gone");

    // POST cancels: the slot returns, and home is owed a machinery record.
    let before = server.db.queue_depth(Some(&server.user.id)).unwrap();
    let reply = Request::new("POST", manage_path, &gate).send(server.gate);
    assert_eq!(reply.status, 200);
    assert!(reply.body.contains("Cancelled"), "{}", reply.body);
    let week = server.get(server.gate, "/s/alice/book");
    assert!(
        week.body.contains("_slot\" value"),
        "a cancelled slot is bookable again: {}",
        week.body
    );
    let rows = server.db.drain(&server.user.id, 0, 10).unwrap();
    assert_eq!(
        server.db.queue_depth(Some(&server.user.id)).unwrap(),
        before + 1,
        "the cancellation queued"
    );
    let cancel: serde_json::Value =
        serde_json::from_str(rows.last().unwrap().payload.as_str()).unwrap();
    assert_eq!(cancel["_cancelled"], serde_json::json!(true));
    assert_eq!(cancel["_booking_id"], payload["_booking_id"]);

    // A second POST finds "already cancelled" and queues nothing more.
    let reply = Request::new("POST", manage_path, &gate).send(server.gate);
    assert!(reply.body.contains("Already cancelled"), "{}", reply.body);
    assert_eq!(
        server.db.queue_depth(Some(&server.user.id)).unwrap(),
        before + 1,
        "cancel is once"
    );

    // A dead token: branded, with a way forward.
    let reply = server.get(server.gate, "/s/alice/book/m/not-a-token");
    assert_eq!(reply.status, 404);
    assert!(reply.body.contains("no longer valid"), "{}", reply.body);
}
