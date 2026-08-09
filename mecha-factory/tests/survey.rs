//! The general poll end to end on the box: home pushes a question spec,
//! the box mints capabilities, ballots validate against the spec's own
//! vocabulary, and results appear exactly when the policy says — enforced
//! where the bytes are emitted, so what a viewer may not see is absent
//! from the page, not hidden on it.

mod common;

use common::{start, Request};
use mecha_factory::db::Scope;

fn spec_push(show: &str, identity: Option<&str>, participants: &[&str]) -> String {
    let mut results = serde_json::json!({ "show": show });
    if let Some(identity) = identity {
        results["identity"] = identity.into();
    }
    serde_json::json!({
        "spec": {
            "title": "Which paper should lab meeting discuss?",
            "questions": [
                {
                    "id": "paper",
                    "kind": "choice",
                    "options": [
                        {"id": "world-models", "label": "World models are enough"},
                        {"id": "affect-probes", "label": "Affective probes"},
                    ],
                },
                {
                    "id": "why",
                    "prompt": "Anything we should know?",
                    "kind": "text",
                    "max_length": 120,
                },
            ],
            "results": results,
        },
        "participants": participants,
    })
    .to_string()
}

#[test]
fn a_survey_reveals_results_after_the_vote_and_refuses_off_vocabulary() {
    let server = start();
    let gate = server.gate.to_string();

    let reply = Request::new("PUT", "/v1/instruments/book/polls/paper-vote", &gate)
        .auth(&server.key(Scope::Slots))
        .body(spec_push("after_vote", None, &["Priya", "Tal"]))
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let urls = reply.json()["urls"].clone();
    let priya = urls["Priya"].as_str().unwrap().to_string();
    let priya_path = &priya[priya.find("/p/").unwrap()..];
    let tal = urls["Tal"].as_str().unwrap().to_string();
    let tal_path = &tal[tal.find("/p/").unwrap()..];

    // Before her vote: the form, the promise, and no results in the bytes.
    let page = server.get(server.gate, priya_path);
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("Hi Priya"), "{}", page.body);
    assert!(page.body.contains("Results appear after you answer."));
    assert!(page.body.contains("name=\"q_paper\""), "{}", page.body);
    assert!(page.body.contains("maxlength=\"120\""));
    assert!(!page.body.contains("tallybar"), "{}", page.body);

    // An answer outside the declared vocabulary saves nothing at all.
    let reply = Request::new("POST", priya_path, &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("q_paper=phlogiston".to_string())
        .send(server.gate);
    assert_eq!(reply.status, 200, "not a redirect: refused, {}", reply.body);
    assert!(reply.body.contains("Nothing was saved"), "{}", reply.body);
    let reply = Request::new("POST", priya_path, &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body("q_paper=phlogiston".to_string())
        .send(server.gate);
    assert_eq!(reply.status, 422, "the autosave is told plainly");

    // Her real ballot: a choice and a capped free line.
    let reply = Request::new("POST", priya_path, &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("q_paper=world-models&q_why=It+has+the+better+ablations".to_string())
        .send(server.gate);
    assert_eq!(reply.status, 303, "{}", reply.body);

    // After her vote the same URL carries the tallies — and her prose,
    // named, because this poll's identity is the default `named`.
    let page = server.get(server.gate, &format!("{priya_path}?saved=1"));
    assert!(page.body.contains("Saved."), "{}", page.body);
    assert!(page.body.contains("tallybar"), "{}", page.body);
    assert!(page.body.contains("<meter max=\"1\" value=\"1\">"), "{}", page.body);
    assert!(page.body.contains("better ablations"), "{}", page.body);
    assert!(page.body.contains("— Priya"), "{}", page.body);

    // Tal has not voted: the reveal is his own, not the poll's.
    let page = server.get(server.gate, tal_path);
    assert!(page.body.contains("1 of 2 have answered"), "{}", page.body);
    assert!(!page.body.contains("tallybar"), "{}", page.body);
    assert!(!page.body.contains("better ablations"), "hidden means absent");

    // The live endpoint answers each viewer by the same policy: Priya's
    // carries the fragments, Tal's carries null — and both say no-store,
    // because the answer is per-viewer and per-moment.
    let live = server.get(server.gate, &format!("{priya_path}/results.json"));
    assert_eq!(live.status, 200, "{}", live.body);
    assert_eq!(live.header("cache-control").as_deref(), Some("no-store"));
    let data = live.json();
    assert_eq!(data["open"], true);
    assert_eq!(data["intro"], "Hi Priya — 1 of 2 have answered so far.");
    let fragment = data["results"]["paper"].as_str().unwrap();
    assert!(fragment.contains("<meter"), "{fragment}");
    assert!(
        data["results"]["why"].as_str().unwrap().contains("better ablations"),
        "{}",
        data["results"]
    );
    let live = server.get(server.gate, &format!("{tal_path}/results.json"));
    assert_eq!(live.status, 200);
    assert!(live.json()["results"].is_null(), "{}", live.body);

    // And the page wires the enhancement in: live form, script, slots.
    let page = server.get(server.gate, priya_path);
    assert!(page.body.contains("data-live=\"1\""), "{}", page.body);
    assert!(page.body.contains("survey.js"), "{}", page.body);
    assert!(page.body.contains("id=\"results-q-paper\""), "{}", page.body);
    let asset = server.get(server.gate, "/p/a/survey.js");
    assert_eq!(asset.status, 200);
    assert!(asset.body.contains("results.json"), "{}", asset.body);

    // Home reads the spec and the tagged ballots back.
    let reply = Request::new("GET", "/v1/instruments/book/polls/paper-vote", &gate)
        .auth(&server.key(Scope::Slots))
        .send(server.gate);
    assert_eq!(reply.status, 200);
    let tally = reply.json();
    assert_eq!(tally["spec"]["questions"][0]["id"], "paper");
    let priya_row = tally["participants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "Priya")
        .unwrap();
    assert_eq!(priya_row["answers"]["paper"]["kind"], "choice");
    assert_eq!(priya_row["answers"]["paper"]["value"][0], "world-models");

    // Close with an outcome: read-only page that answers "so what
    // happened?", frozen ballots, the late autosave refused.
    let reply = Request::new("POST", "/v1/instruments/book/polls/paper-vote/close", &gate)
        .auth(&server.key(Scope::Slots))
        .body(r#"{"resolution": "World models it is — Thursday, usual room."}"#.to_string())
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let page = server.get(server.gate, priya_path);
    assert!(page.body.contains("closed"), "{}", page.body);
    assert!(
        page.body.contains("Outcome:") && page.body.contains("usual room"),
        "{}",
        page.body
    );
    let reply = Request::new("GET", "/v1/instruments/book/polls/paper-vote", &gate)
        .auth(&server.key(Scope::Slots))
        .send(server.gate);
    assert!(
        reply.json()["resolution"].as_str().unwrap().contains("Thursday"),
        "{}",
        reply.body
    );
    assert!(!page.body.contains("<button type=\"submit\""), "{}", page.body);
    let reply = Request::new("POST", priya_path, &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body("q_paper=affect-probes".to_string())
        .send(server.gate);
    assert_eq!(reply.status, 409);
    // Closed is a state the live endpoint reports, not an error.
    let live = server.get(server.gate, &format!("{priya_path}/results.json"));
    assert_eq!(live.json()["open"], false);
}

#[test]
fn a_times_poll_has_no_results_endpoint() {
    let server = start();
    let gate = server.gate.to_string();
    let reply = Request::new("PUT", "/v1/instruments/book/polls/lab-feb", &gate)
        .auth(&server.key(Scope::Slots))
        .body(
            serde_json::json!({
                "title": "Lab meeting",
                "timezone": "America/New_York",
                "duration_minutes": 60,
                "candidates": [
                    {"start": "2030-02-05T18:00:00Z", "end": "2030-02-05T19:00:00Z",
                     "duration_minutes": 60},
                ],
                "participants": ["Priya"],
            })
            .to_string(),
        )
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let url = reply.json()["urls"]["Priya"].as_str().unwrap().to_string();
    let path = &url[url.find("/p/").unwrap()..];
    assert_eq!(
        server
            .get(server.gate, &format!("{path}/results.json"))
            .status,
        404,
        "a times poll's heat rides its page"
    );
}

#[test]
fn an_anonymous_poll_suppresses_small_cells_and_never_names_anyone() {
    let server = start();
    let gate = server.gate.to_string();

    let reply = Request::new("PUT", "/v1/instruments/book/polls/pulse", &gate)
        .auth(&server.key(Scope::Slots))
        .body(spec_push("live", Some("anonymous"), &["Priya", "Tal", "Noor"]))
        .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let urls = reply.json()["urls"].clone();
    let path_of = |name: &str| {
        let url = urls[name].as_str().unwrap().to_string();
        url[url.find("/p/").unwrap()..].to_string()
    };

    // One ballot in: below the floor, the breakdown is withheld — from
    // everyone, prose included.
    let reply = Request::new("POST", &path_of("Priya"), &gate)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("q_paper=world-models&q_why=please+not+another+transformer".to_string())
        .send(server.gate);
    assert_eq!(reply.status, 303, "{}", reply.body);
    let page = server.get(server.gate, &path_of("Tal"));
    assert!(page.body.contains("not to the organizer"), "{}", page.body);
    assert!(
        page.body.contains("too few to break down"),
        "{}",
        page.body
    );
    assert!(
        !page.body.contains("another transformer"),
        "one anonymous voice is not an aggregate"
    );

    // Three ballots in: the tallies show, the prose shows, no names ever.
    for name in ["Tal", "Noor"] {
        let reply = Request::new("POST", &path_of(name), &gate)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("q_paper=affect-probes".to_string())
            .send(server.gate);
        assert_eq!(reply.status, 303, "{}", reply.body);
    }
    let page = server.get(server.gate, &path_of("Priya"));
    assert!(page.body.contains("tallybar"), "{}", page.body);
    assert!(page.body.contains("another transformer"), "{}", page.body);
    assert!(
        !page.body.contains("— Priya") && !page.body.contains("Who answered what"),
        "anonymous results carry no names: {}",
        page.body
    );

    // The drain obeys the same policy: ballots ride nameless and without
    // timestamps, while who-has-answered stays readable from the roster —
    // here everyone answered, so no row carries a name at all.
    let reply = Request::new("GET", "/v1/instruments/book/polls/pulse", &gate)
        .auth(&server.key(Scope::Slots))
        .send(server.gate);
    assert_eq!(reply.status, 200);
    let rows = reply.json()["participants"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert!(row["name"].is_null(), "an anonymous ballot is nameless: {row}");
        assert!(
            row["responded_at"].is_null(),
            "a timestamp is a correlator too: {row}"
        );
        assert!(!row["answers"].is_null());
    }
}

#[test]
fn a_spec_push_refuses_the_shapes_the_design_names() {
    let server = start();
    let gate = server.gate.to_string();
    let put = |id: &str, body: String| {
        Request::new("PUT", &format!("/v1/instruments/book/polls/{id}"), &gate)
            .auth(&server.key(Scope::Slots))
            .body(body)
            .send(server.gate)
    };

    // A spec and candidates together: two sources of truth, refused.
    let mut both: serde_json::Value =
        serde_json::from_str(&spec_push("live", None, &["Priya"])).unwrap();
    both["candidates"] = serde_json::json!([
        {"start": "2030-02-05T18:00:00Z", "end": "2030-02-05T19:00:00Z", "duration_minutes": 60},
    ]);
    let reply = put("both", both.to_string());
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(reply.body.contains("never both"), "{}", reply.body);

    // A times question inside a spec: the seeded push owns that shape.
    let times = serde_json::json!({
        "spec": {
            "title": "When",
            "questions": [
                {"id": "when", "kind": "times", "timezone": "America/New_York",
                 "duration_minutes": 60},
            ],
        },
        "participants": ["Priya"],
    });
    let reply = put("times-in-spec", times.to_string());
    assert_eq!(reply.status, 400, "{}", reply.body);

    // A link audience: not served yet, refused rather than half-served.
    let mut linked: serde_json::Value =
        serde_json::from_str(&spec_push("live", None, &["Priya"])).unwrap();
    linked["spec"]["audience"] = serde_json::json!({"kind": "link", "max_ballots": 100});
    linked["spec"]["results"] = serde_json::json!({"identity": "anonymous"});
    let reply = put("linked", linked.to_string());
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(reply.body.contains("roster"), "{}", reply.body);

    // A spec that fails its own check is refused with the check's words.
    let mut short: serde_json::Value =
        serde_json::from_str(&spec_push("live", None, &["Priya"])).unwrap();
    short["spec"]["questions"][0]["options"] = serde_json::json!([
        {"id": "only", "label": "Only one"},
    ]);
    let reply = put("short", short.to_string());
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(reply.body.contains("fewer than two"), "{}", reply.body);
}
