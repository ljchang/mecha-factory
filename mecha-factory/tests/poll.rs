//! The group poll end to end on the box: home pushes seeded candidates and
//! names, the box mints one capability per participant, each answers in
//! three enum values, home reads the tally — and every wrong door answers
//! nothing.

mod common;

use common::{start, Request};
use mecha_factory::db::Scope;

fn push_body() -> String {
    serde_json::json!({
        "title": "Lab meeting",
        "timezone": "America/New_York",
        "duration_minutes": 60,
        "deadline": "2030-01-01T00:00:00Z",
        "candidates": [
            {"start": "2030-02-05T18:00:00Z", "end": "2030-02-05T19:00:00Z", "duration_minutes": 60},
            {"start": "2030-02-06T15:00:00Z", "end": "2030-02-06T16:00:00Z", "duration_minutes": 60},
        ],
        "participants": ["Priya", "Tal"],
    })
    .to_string()
}

#[test]
fn a_poll_collects_tristate_answers_behind_capabilities() {
    let server = start();
    let gate = server.gate.to_string();

    // Create: the box answers with one URL per participant.
    let reply = Request::new("PUT", "/v1/instruments/book/polls/lab-feb", &gate)
        .auth(&server.key(Scope::Slots))
        .body(push_body())
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let urls = reply.json()["urls"].clone();
    let priya = urls["Priya"].as_str().unwrap().to_string();
    let priya_path = &priya[priya.find("/p/").unwrap()..];

    // A second create with the same id is refused: links are in inboxes.
    let reply = Request::new("PUT", "/v1/instruments/book/polls/lab-feb", &gate)
        .auth(&server.key(Scope::Slots))
        .body(push_body())
        .send(server.gate);
    assert_eq!(reply.status, 409, "{}", reply.body);

    // Priya's page: her name, the seeded candidates, the host zone.
    let page = server.get(server.gate, priya_path);
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("Hi Priya"), "{}", page.body);
    assert!(page.body.contains("0 of 2 have answered"));
    assert!(page.body.contains("a_2030-02-05T18:00:00Z|60"));
    assert!(page.body.contains("Times are shown in America/New_York"));

    // She answers: yes to Wednesday, if-needed to Thursday.
    let reply = Request::new("POST", priya_path, &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(
            "a_2030-02-05T18%3A00%3A00Z%7C60=yes&a_2030-02-06T15%3A00%3A00Z%7C60=if_needed"
                .to_string(),
        )
        .send(server.gate);
    assert_eq!(reply.status, 303, "a save redirects: {}", reply.body);

    // Her answers persist and render back checked; the count moved.
    let page = server.get(server.gate, &format!("{priya_path}?saved=1"));
    assert!(page.body.contains("Saved."), "{}", page.body);
    assert!(page.body.contains("1 of 2 have answered"));
    assert!(page.body.contains("value=\"yes\" checked"), "{}", page.body);

    // Tal's page shows Priya's heat, not her identity.
    let tal = urls["Tal"].as_str().unwrap().to_string();
    let tal_path = &tal[tal.find("/p/").unwrap()..];
    let page = server.get(server.gate, tal_path);
    assert!(page.body.contains("1 of 2 yes"), "{}", page.body);
    assert!(
        page.body.contains("heat-3"),
        "one of two yeses shades the cell: {}",
        page.body
    );
    assert!(
        !page.body.contains("Priya"),
        "names are not each other's business"
    );

    // Home reads the tally, typed.
    let reply = Request::new("GET", "/v1/instruments/book/polls/lab-feb", &gate)
        .auth(&server.key(Scope::Slots))
        .send(server.gate);
    assert_eq!(reply.status, 200);
    let tally = reply.json();
    assert_eq!(tally["state"], "open");
    let priya_row = tally["participants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "Priya")
        .unwrap();
    assert_eq!(priya_row["answers"]["2030-02-05T18:00:00Z|60"], "yes");

    // An autosave (poll.js asking for JSON) is answered with a bare 204 —
    // no redirect for a fetch to follow, nothing to render.
    let reply = Request::new("POST", priya_path, &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body("a_2030-02-05T18%3A00%3A00Z%7C60=yes".to_string())
        .send(server.gate);
    assert_eq!(reply.status, 204, "an autosave saves: {}", reply.body);

    // Close: answers freeze, the page says so, a late POST changes nothing.
    let reply = Request::new("POST", "/v1/instruments/book/polls/lab-feb/close", &gate)
        .auth(&server.key(Scope::Slots))
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let page = server.get(server.gate, priya_path);
    assert!(page.body.contains("closed"), "{}", page.body);
    let reply = Request::new("POST", priya_path, &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("a_2030-02-05T18%3A00%3A00Z%7C60=no".to_string())
        .send(server.gate);
    assert_eq!(reply.status, 200, "not a redirect: nothing was saved");
    // The autosave shape of the same late POST is refused in a word.
    let reply = Request::new("POST", priya_path, &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body("a_2030-02-05T18%3A00%3A00Z%7C60=no".to_string())
        .send(server.gate);
    assert_eq!(reply.status, 409, "a closed poll tells the script plainly");
    let reply = Request::new("GET", "/v1/instruments/book/polls/lab-feb", &gate)
        .auth(&server.key(Scope::Slots))
        .send(server.gate);
    assert_eq!(
        reply.json()["participants"][0]["answers"]["2030-02-05T18:00:00Z|60"],
        "yes",
        "the close froze the answer"
    );
}

#[test]
fn wrong_doors_and_bad_pushes_answer_nothing() {
    let server = start();
    let gate = server.gate.to_string();
    let reply = Request::new("PUT", "/v1/instruments/book/polls/lab-feb", &gate)
        .auth(&server.key(Scope::Slots))
        .body(push_body())
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let urls = reply.json()["urls"].clone();
    let priya = urls["Priya"].as_str().unwrap().to_string();
    let token = priya.rsplit('/').next().unwrap();

    // A token spliced onto another handle or poll id: nothing.
    let bob = server.add_user("bob");
    let _ = bob;
    assert_eq!(
        server
            .get(server.gate, &format!("/p/bob/lab-feb/{token}"))
            .status,
        404
    );
    assert_eq!(
        server
            .get(server.gate, &format!("/p/alice/other-poll/{token}"))
            .status,
        404
    );
    assert_eq!(
        server
            .get(server.gate, "/p/alice/lab-feb/not-a-token")
            .status,
        404
    );

    // Only the slots key creates polls; a publish key is refused.
    let reply = Request::new("PUT", "/v1/instruments/book/polls/other", &gate)
        .auth(&server.key(Scope::Publish))
        .body(push_body())
        .send(server.gate);
    assert_eq!(reply.status, 403);

    // A push whose candidate disagrees with itself is refused whole.
    let mut bad: serde_json::Value = serde_json::from_str(&push_body()).unwrap();
    bad["candidates"][0]["duration_minutes"] = serde_json::json!(30);
    let reply = Request::new("PUT", "/v1/instruments/book/polls/bad", &gate)
        .auth(&server.key(Scope::Slots))
        .body(bad.to_string())
        .send(server.gate);
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(reply.body.contains("disagree"), "{}", reply.body);
}
