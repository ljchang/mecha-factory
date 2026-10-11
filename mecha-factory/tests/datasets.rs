//! A released dashboard's datasets: home pushing rows beside an immutable
//! version, and every push the box refuses — against a real server, with real
//! keys (`mecha/docs/LIVE-DASHBOARD-DESIGN.md` §6.2).

mod common;

use common::{start, Request, Server};
use mecha_factory::db::{BundleRow, Scope, ViewCapRow};
use mecha_manifest::{ContentClass, Visibility};

const LOADER: &str = "sha256:loader-v1";

/// A dashboard version on disk and in the ledger, declaring one dataset
/// `visits` (day, visits) released from `LOADER`, aliased at `version`.
fn dashboard(server: &Server, id: &str, version: u32, visibility: Visibility) {
    bundle_of(server, id, version, visibility, ContentClass::Interactive)
}

fn bundle_of(server: &Server, id: &str, version: u32, visibility: Visibility, class: ContentClass) {
    let dir = server
        .dir
        .path()
        .join("bundles")
        .join(&server.user.id)
        .join(id)
        .join(version.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), "<div id=hud></div>").unwrap();
    std::fs::write(
        dir.join(mecha_manifest::hud::DATASETS_FILE),
        serde_json::json!({ "datasets": { "visits": {
            "columns": [{"name": "day", "type": "date"}, {"name": "visits", "type": "integer"}],
            "loader_digest": LOADER,
        }}})
        .to_string(),
    )
    .unwrap();
    server
        .db
        .bundle_insert(&BundleRow {
            user_id: server.user.id.clone(),
            id: id.into(),
            version,
            digest: format!("sha256:{id}{version}"),
            class,
            title: id.into(),
            description: None,
            template: "hud".into(),
            published_at: Some("2031-04-17T07:00:00Z".into()),
            received_at: "2031-04-17T07:00:01Z".into(),
            withheld_at: None,
            withheld_reason: None,
        })
        .unwrap();
    server
        .db
        .alias_set(
            &server.user.id,
            id,
            Some(version),
            visibility,
            "2031-04-17T07:00:02Z",
        )
        .unwrap();
}

fn push(release: u32, generation: u64, visits: i64) -> String {
    serde_json::json!({
        "release": release,
        "generation": generation,
        "generated_at": "2031-04-17T09:00:00Z",
        "loader_digest": LOADER,
        "columns": [{"name": "day", "type": "date"}, {"name": "visits", "type": "integer"}],
        "rows": [["2031-04-17", visits]],
    })
    .to_string()
}

fn put(server: &Server, key: &str, id: &str, name: &str, body: String) -> common::Reply {
    Request::new(
        "PUT",
        &format!("/v1/bundles/{id}/datasets/{name}"),
        server.gate.to_string(),
    )
    .auth(key)
    .body(body)
    .send(server.gate)
}

fn read(server: &Server, path: &str) -> common::Reply {
    server.get_as(server.artifacts, &server.user.handle, path)
}

#[test]
fn a_data_key_replaces_a_dataset_and_the_page_reads_it_beside_the_version() {
    let server = start();
    dashboard(&server, "lab", 1, Visibility::Public);
    let key = server.key(Scope::Data);

    let reply = put(&server, &key, "lab", "visits", push(1, 1, 7));
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["stored"], true);

    let page = read(&server, "/b/lab/data/visits.json");
    assert_eq!(page.status, 200, "{}", page.body);
    let body: serde_json::Value = serde_json::from_str(&page.body).unwrap();
    assert_eq!(body["rows"][0][1], 7);
    assert_eq!(body["name"], "visits");
    assert!(body.get("release").is_none(), "the page reads home's shape");
    let tag = page.header("etag").expect("an etag");
    assert_eq!(
        page.header("cache-control").as_deref(),
        Some("public, no-cache")
    );

    // Revalidated by the tag: unchanged rows cost a 304.
    let again = Request::new(
        "GET",
        "/b/lab/data/visits.json",
        server.host_for(server.artifacts, &server.user.handle),
    )
    .header("If-None-Match", &tag)
    .send(server.artifacts);
    assert_eq!(again.status, 304, "{}", again.head);

    // A newer generation replaces it, and the tag moves.
    assert_eq!(
        put(&server, &key, "lab", "visits", push(1, 2, 9)).status,
        200
    );
    let page = read(&server, "/b/lab/data/visits.json");
    assert!(page.body.contains("9"), "{}", page.body);
    assert_ne!(page.header("etag"), Some(tag));
}

#[test]
fn every_other_scope_is_refused_and_so_is_no_key() {
    let server = start();
    dashboard(&server, "lab", 1, Visibility::Public);
    for scope in [Scope::Publish, Scope::Release, Scope::Drain, Scope::Slots] {
        let reply = put(&server, &server.key(scope), "lab", "visits", push(1, 1, 7));
        assert_eq!(reply.status, 403, "{}: {}", scope.as_str(), reply.body);
    }
    let reply = Request::new(
        "PUT",
        "/v1/bundles/lab/datasets/visits",
        server.gate.to_string(),
    )
    .body(push(1, 1, 7))
    .send(server.gate);
    assert_eq!(reply.status, 401, "{}", reply.body);
}

/// The order of pushes within a release, and the retry that must be harmless.
#[test]
fn generations_order_pushes_and_a_retry_is_an_idempotent_success() {
    let server = start();
    dashboard(&server, "lab", 1, Visibility::Public);
    let key = server.key(Scope::Data);
    assert_eq!(
        put(&server, &key, "lab", "visits", push(1, 5, 7)).status,
        200
    );

    // The same push again — the retry after a timeout.
    let retry = put(&server, &key, "lab", "visits", push(1, 5, 7));
    assert_eq!(retry.status, 200, "{}", retry.body);
    assert_eq!(retry.json()["stored"], false);

    let older = put(&server, &key, "lab", "visits", push(1, 4, 99));
    assert_eq!(older.status, 409, "{}", older.body);
    assert_eq!(older.json()["code"], "order");

    let forked = put(&server, &key, "lab", "visits", push(1, 5, 99));
    assert_eq!(forked.status, 409, "{}", forked.body);
    assert_eq!(forked.json()["code"], "forked");

    let row = server
        .db
        .dataset_get(&server.user.id, "lab", "visits")
        .unwrap()
        .unwrap();
    assert!(row.body.contains("[\"2031-04-17\",7]"), "{}", row.body);
}

/// A push names the release it was made under; any other is stale, and a new
/// release starts the counter over and drops the old release's rows.
#[test]
fn a_push_for_another_release_is_stale_and_a_new_release_starts_over() {
    let server = start();
    dashboard(&server, "lab", 1, Visibility::Public);
    let key = server.key(Scope::Data);
    assert_eq!(
        put(&server, &key, "lab", "visits", push(1, 9, 7)).status,
        200
    );

    // Republished: the alias moves to version 2.
    dashboard(&server, "lab", 2, Visibility::Public);
    let stale = put(&server, &key, "lab", "visits", push(1, 10, 8));
    assert_eq!(stale.status, 409, "{}", stale.body);
    assert_eq!(stale.json()["code"], "stale");

    // Nothing pushed under release 2 yet: the page has no rows, never
    // release 1's.
    assert_eq!(read(&server, "/b/lab/data/visits.json").status, 404);

    // The seed: generation 1 under the new release, below the old counter.
    let seeded = put(&server, &key, "lab", "visits", push(2, 1, 3));
    assert_eq!(seeded.status, 200, "{}", seeded.body);
    let page = read(&server, "/b/lab/data/visits.json");
    assert_eq!(page.status, 200);
    assert!(page.body.contains("[\"2031-04-17\",3]"), "{}", page.body);
}

/// What runs unattended is exactly what was released: the loader, the
/// columns, and a dataset the release declared.
#[test]
fn a_push_off_the_released_loader_or_shape_is_refused() {
    let server = start();
    dashboard(&server, "lab", 1, Visibility::Public);
    let key = server.key(Scope::Data);

    let other_loader = push(1, 1, 7).replace(LOADER, "sha256:edited");
    let reply = put(&server, &key, "lab", "visits", other_loader);
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert_eq!(reply.json()["code"], "loader");

    let other_shape = push(1, 1, 7).replace(
        "\"visits\",\"type\":\"integer\"",
        "\"visits\",\"type\":\"float\"",
    );
    let reply = put(&server, &key, "lab", "visits", other_shape);
    assert_eq!(reply.status, 422, "{}", reply.body);
    assert_eq!(reply.json()["code"], "shape");

    let short_row = push(1, 1, 7).replace("[\"2031-04-17\",7]", "[\"2031-04-17\"]");
    let reply = put(&server, &key, "lab", "visits", short_row);
    assert_eq!(reply.status, 422, "{}", reply.body);

    let reply = put(&server, &key, "lab", "instruments", push(1, 1, 7));
    assert_eq!(reply.status, 422, "{}", reply.body);
    assert_eq!(reply.json()["code"], "undeclared");

    for (id, name) in [("lab", "../x"), ("lab", "Visits"), ("../lab", "visits")] {
        let reply = put(&server, &key, id, name, push(1, 1, 7));
        assert!(
            matches!(reply.status, 400 | 404),
            "{id}/{name}: {} {}",
            reply.status,
            reply.body
        );
    }

    let reply = put(&server, &key, "nothing", "visits", push(1, 1, 7));
    assert_eq!(reply.status, 404, "{}", reply.body);
    assert!(server
        .db
        .dataset_get(&server.user.id, "lab", "visits")
        .unwrap()
        .is_none());
}

/// One push and the tenant's datasets together are capped.
#[test]
fn a_dataset_over_its_cap_is_refused() {
    let server = start();
    dashboard(&server, "lab", 1, Visibility::Public);
    let key = server.key(Scope::Data);
    // Just over the 4 MiB cap on one dataset, and under the route's body
    // limit (the cap plus slack), so the handler's own check is the one that
    // answers.
    let cap = 4 * 1024 * 1024;
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let body = loop {
        rows.extend((0..1000).map(|i| serde_json::json!(["2031-04-17", i])));
        let body = serde_json::json!({
            "release": 1, "generation": 1, "generated_at": "2031-04-17T09:00:00Z",
            "loader_digest": LOADER,
            "columns": [{"name": "day", "type": "date"}, {"name": "visits", "type": "integer"}],
            "rows": rows,
        })
        .to_string();
        if body.len() > cap + 1024 {
            break body;
        }
    };
    assert!(body.len() < cap + 64 * 1024);
    let reply = put(&server, &key, "lab", "visits", body);
    assert_eq!(reply.status, 413, "{}", reply.body);
    assert_eq!(reply.json()["code"], "size");
}

/// A private dashboard's data: nothing on the public path, and the rows under
/// the grant that opened the page (R16) — of the current release, whatever
/// version the grant names.
#[test]
fn a_private_dashboards_data_is_read_under_its_grant_only() {
    let server = start();
    dashboard(&server, "lab", 1, Visibility::Private);
    let key = server.key(Scope::Data);
    assert_eq!(
        put(&server, &key, "lab", "visits", push(1, 1, 7)).status,
        200
    );

    assert_eq!(read(&server, "/b/lab/data/visits.json").status, 404);

    let cap = "ab".repeat(16);
    server
        .db
        .view_cap_create(
            &ViewCapRow {
                user_id: server.user.id.clone(),
                bundle_id: "lab".into(),
                version: 1,
                email: String::new(),
            },
            &mecha_factory::intake::hash_token(&cap),
            "2031-04-17T07:00:00Z",
            "2999-01-01T00:00:00Z",
        )
        .unwrap();
    let page = read(&server, &format!("/g/{cap}/data/visits.json"));
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("[\"2031-04-17\",7]"));
    assert_eq!(
        page.header("cache-control").as_deref(),
        Some("private, no-cache")
    );

    // A bad name under the grant is a 404, never a file.
    assert_eq!(
        read(&server, &format!("/g/{cap}/data/..%2Fx.json")).status,
        404
    );
}

/// A version that declares no datasets keeps every path to its own files.
#[test]
fn a_bundle_that_is_not_a_dashboard_has_no_data_path() {
    let server = start();
    dashboard(&server, "lab", 1, Visibility::Public);
    let key = server.key(Scope::Data);
    assert_eq!(
        put(&server, &key, "lab", "visits", push(1, 1, 7)).status,
        200
    );
    let declared = server
        .dir
        .path()
        .join("bundles")
        .join(&server.user.id)
        .join("lab/1")
        .join(mecha_manifest::hud::DATASETS_FILE);
    std::fs::remove_file(declared).unwrap();
    assert_eq!(read(&server, "/b/lab/data/visits.json").status, 404);
}

#[test]
fn a_reset_drops_a_dashboards_datasets() {
    let server = start();
    dashboard(&server, "lab", 1, Visibility::Public);
    let key = server.key(Scope::Data);
    assert_eq!(
        put(&server, &key, "lab", "visits", push(1, 9, 7)).status,
        200
    );
    let reply = Request::new(
        "DELETE",
        "/v1/bundles/lab/datasets",
        server.gate.to_string(),
    )
    .auth(&key)
    .send(server.gate);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["removed"], 1);
    // A home that lost its counter starts again from 1.
    assert_eq!(
        put(&server, &key, "lab", "visits", push(1, 1, 3)).status,
        200
    );
}

/// The class decides the origin for the rows as for the page: an interactive
/// dashboard's data is not served on the compute origin, and a bundle that is
/// not an interactive dashboard takes no pushes (review of #24).
#[test]
fn a_dashboards_rows_answer_only_on_its_classs_origin() {
    let server = start();
    dashboard(&server, "lab", 1, Visibility::Public);
    let key = server.key(Scope::Data);
    assert_eq!(
        put(&server, &key, "lab", "visits", push(1, 1, 7)).status,
        200
    );
    assert_eq!(read(&server, "/b/lab/data/visits.json").status, 200);
    let compute = server.get_as(
        server.compute,
        &server.user.handle,
        "/b/lab/data/visits.json",
    );
    assert_eq!(compute.status, 404, "{}", compute.head);

    bundle_of(
        &server,
        "report",
        1,
        Visibility::Public,
        ContentClass::Static,
    );
    let reply = put(&server, &key, "report", "visits", push(1, 1, 7));
    assert_eq!(reply.status, 422, "{}", reply.body);
    assert_eq!(reply.json()["code"], "undeclared");
}

/// The tenant's datasets together are capped: the budget that bounds a key
/// pushing unattended.
#[test]
fn a_push_past_the_tenants_dataset_budget_is_refused() {
    let server = start();
    dashboard(&server, "lab", 1, Visibility::Public);
    // Another dashboard's rows already hold all but a few bytes of the
    // 64 MiB budget.
    server
        .db
        .dataset_put(&mecha_factory::db::DatasetRow {
            user_id: server.user.id.clone(),
            bundle_id: "other".into(),
            name: "big".into(),
            release: 1,
            generation: 1,
            digest: "d".into(),
            generated_at: "2031-04-17T09:00:00Z".into(),
            body: "x".repeat(64 * 1024 * 1024 - 64),
            received_at: "2031-04-17T09:00:00Z".into(),
        })
        .unwrap();
    let reply = put(
        &server,
        &server.key(Scope::Data),
        "lab",
        "visits",
        push(1, 1, 7),
    );
    assert_eq!(reply.status, 413, "{}", reply.body);
    assert_eq!(reply.json()["code"], "budget");
}

/// A withhold covers the rows of the release they belong to: a grant minted
/// for an older version cannot read what a withheld release pushed.
#[test]
fn a_withheld_release_s_rows_are_not_read_through_an_older_grant() {
    let server = start();
    dashboard(&server, "lab", 1, Visibility::Private);
    let cap = "cd".repeat(16);
    server
        .db
        .view_cap_create(
            &ViewCapRow {
                user_id: server.user.id.clone(),
                bundle_id: "lab".into(),
                version: 1,
                email: String::new(),
            },
            &mecha_factory::intake::hash_token(&cap),
            "2031-04-17T07:00:00Z",
            "2999-01-01T00:00:00Z",
        )
        .unwrap();
    dashboard(&server, "lab", 2, Visibility::Private);
    let key = server.key(Scope::Data);
    assert_eq!(
        put(&server, &key, "lab", "visits", push(2, 1, 7)).status,
        200
    );
    let path = format!("/g/{cap}/data/visits.json");
    assert_eq!(
        read(&server, &path).status,
        200,
        "the grant reads the current release"
    );

    server
        .db
        .bundle_withhold(
            &server.user.id,
            "lab",
            2,
            Some("a report"),
            Some("2031-04-17T10:00:00Z"),
        )
        .unwrap();
    assert_eq!(read(&server, &path).status, 404);
}

/// A version that declares no datasets keeps its own `data/` files, under a
/// grant as on the public path: the dataset route falls through.
#[test]
fn a_version_that_declares_no_datasets_serves_its_own_data_files_under_a_grant() {
    let server = start();
    dashboard(&server, "lab", 1, Visibility::Private);
    let version = server
        .dir
        .path()
        .join("bundles")
        .join(&server.user.id)
        .join("lab/1");
    std::fs::remove_file(version.join(mecha_manifest::hud::DATASETS_FILE)).unwrap();
    std::fs::create_dir_all(version.join("data")).unwrap();
    std::fs::write(version.join("data/table.json"), r#"{"own":"file"}"#).unwrap();
    let cap = "ef".repeat(16);
    server
        .db
        .view_cap_create(
            &ViewCapRow {
                user_id: server.user.id.clone(),
                bundle_id: "lab".into(),
                version: 1,
                email: String::new(),
            },
            &mecha_factory::intake::hash_token(&cap),
            "2031-04-17T07:00:00Z",
            "2999-01-01T00:00:00Z",
        )
        .unwrap();
    let page = read(&server, &format!("/g/{cap}/data/table.json"));
    assert_eq!(page.status, 200, "{}", page.head);
    assert!(page.body.contains("own"), "{}", page.body);
}

/// A withheld release is fed by nobody, as it is served to nobody.
#[test]
fn a_withheld_release_takes_no_pushes() {
    let server = start();
    dashboard(&server, "lab", 1, Visibility::Public);
    server
        .db
        .bundle_withhold(
            &server.user.id,
            "lab",
            1,
            Some("a report"),
            Some("2031-04-17T10:00:00Z"),
        )
        .unwrap();
    let reply = put(
        &server,
        &server.key(Scope::Data),
        "lab",
        "visits",
        push(1, 1, 7),
    );
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert_eq!(reply.json()["code"], "withheld");
}
