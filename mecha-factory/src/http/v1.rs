//! The API, on the gate origin only.
//!
//! Six endpoints, JSON in and out, bearer token. Deliberately boring — the
//! interesting decisions are all in what they refuse.

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use mecha_manifest::Visibility;

use super::{Failure, Shared};
use crate::config::{Origin, Role};
use crate::db::{KeyRow, Scope, UserRow};
use crate::keys;

/// Read the bearer header, if there is one.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
}

/// Only the gate serves the API. Anywhere else it does not exist.
///
/// Returns the refusal rather than taking a `Result`, so the call site reads
/// as one line and there is no error type large enough for a whole `Response`
/// to travel in.
pub fn not_on_gate(origin: &Origin) -> Option<Response> {
    (origin.role != Role::Gate)
        .then(|| Failure::text(StatusCode::NOT_FOUND, "not found").into_response())
}

/// `GET /v1/health` — is it up, what version, how many queued.
///
/// Public, because the thing that watches it is a trigger that must cost
/// nothing and must work when every key has just been rotated. **The counts are
/// not public**: queue depth is a fact about how many strangers wrote to us this
/// week, and a health check is not a reason to publish it. A caller holding any
/// live key gets the detail.
pub async fn health(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    let mut body = serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_s": app.started.elapsed().as_secs(),
    });

    // The counts a key gets are **its own user's**, not the box's. How many
    // reports somebody else published is not a fact this endpoint owes anyone,
    // and a health check that leaked it would be the one place tenancy
    // silently did not hold.
    if let Ok(row) = keys::authenticate_any(&app.db, bearer(&headers)) {
        let queued = app.db.queue_depth(Some(&row.user_id)).unwrap_or(-1);
        body["queued"] = queued.into();
        body["key"] = row.id.clone().into();
        if let Ok(Some(user)) = app.db.user(&row.user_id) {
            body["handle"] = user.handle.into();
            body["status"] = user.status.into();
        }
    }
    (StatusCode::OK, Json(body)).into_response()
}

/// Authenticate, or hand back the refusal to return.
///
/// Logs which key acted, because rotation and revocation are only reviewable
/// after the fact if there is a record of what a key did while it was live.
fn authorised(
    app: &Shared,
    headers: &HeaderMap,
    needs: Scope,
) -> Result<(KeyRow, UserRow), Box<Response>> {
    match keys::authenticate(&app.db, bearer(headers), needs) {
        Ok(row) => {
            // A live key belonging to a suspended user is refused here, once,
            // rather than in each handler — suspension has to mean the account
            // stops working, not that most of it does.
            let user = match app.db.user(&row.user_id) {
                Ok(Some(user)) if user.active() => user,
                Ok(Some(user)) => {
                    tracing::warn!(key = %row.id, handle = %user.handle, "suspended account");
                    return Err(Box::new(
                        Failure::json(StatusCode::FORBIDDEN, "this account is suspended")
                            .into_response(),
                    ));
                }
                _ => {
                    tracing::error!(key = %row.id, user = %row.user_id, "key with no user");
                    return Err(Box::new(
                        Failure::json(StatusCode::UNAUTHORIZED, "a valid bearer token is required")
                            .into_response(),
                    ));
                }
            };
            tracing::info!(
                key = %row.id,
                handle = %user.handle,
                scope = row.scope.as_str(),
                "authenticated"
            );
            Ok((row, user))
        }
        Err(e) => {
            tracing::warn!(error = %e, "refused");
            let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::UNAUTHORIZED);
            Err(Box::new(
                Failure::json(status, e.public_message()).into_response(),
            ))
        }
    }
}

/// `POST /v1/bundles` — publish a version.
///
/// The body is a tar (optionally gzipped) of the rendered directory, with its
/// `bundle.json` inside. **Home decides the version number**, because home
/// computes the content address before the POST and therefore already knows the
/// URL it is about to hand the user; a server that renumbered would make the
/// staged outbox item's promised URL a lie.
///
/// Three answers, and only one of them writes anything:
///
/// - identical bytes already stored under this id → that version, `existing`
/// - the claimed version exists with *different* bytes → 409, and nothing is
///   touched. A version is written once.
/// - otherwise → the new version, installed and indexed.
///
/// The first case is what makes a retry safe, and it is the same property the
/// `Idempotency-Key` header buys — kept anyway, because a retry after a
/// timeout may be re-rendering rather than re-sending, and two renders of the
/// same report are not always byte-identical.
pub async fn publish(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    let user = match authorised(&app, &headers, Scope::Publish) {
        Ok((_, user)) => user,
        Err(refusal) => return *refusal,
    };
    let idempotency = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.len() <= 200);

    if let Some(key) = &idempotency {
        if let Ok(Some((id, version))) = app.db.idempotent(&user.id, key) {
            tracing::info!(%id, version, "a retry of a publish that already landed");
            return published(&app, &user, &id, version, true, StatusCode::OK);
        }
    }

    // Unpacking is CPU and disk work on a body that may be hundreds of
    // megabytes, so it does not happen on a runtime worker.
    let limits = app.config.limits.clone();
    let incoming = match tokio::task::spawn_blocking(move || crate::upload::unpack(&body, &limits))
        .await
    {
        Ok(Ok(incoming)) => incoming,
        Ok(Err(rejected)) => {
            tracing::warn!(error = %rejected, "refused a bundle");
            return Failure::json(StatusCode::BAD_REQUEST, rejected.to_string()).into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "unpacking panicked");
            return Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    };

    let id = incoming.manifest.id.clone();
    let version = incoming.manifest.version;

    match app.db.bundle_by_digest(&user.id, &id, &incoming.digest) {
        Ok(Some(row)) => {
            if let Some(key) = &idempotency {
                let _ =
                    app.db
                        .idempotency_record(&user.id, key, &id, row.version, &crate::db::now());
            }
            tracing::info!(%id, version = row.version, "identical bytes; nothing minted");
            return published(&app, &user, &id, row.version, true, StatusCode::OK);
        }
        Ok(None) => {}
        Err(e) => {
            tracing::error!(%id, error = %e, "reading the ledger");
            return Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    }

    if let Ok(Some(existing)) = app.db.bundle(&user.id, &id, version) {
        return Failure::json(
            StatusCode::CONFLICT,
            format!(
                "{id} version {version} is already published, with different bytes \
                 ({} here, {} offered). A version is written once; publish the next one.",
                existing.digest, incoming.digest
            ),
        )
        .into_response();
    }

    // The quota, checked before the bytes land rather than after. A per-user
    // limit is what stops one held key filling a disk everybody shares — a
    // global cap stopped being a cap the moment the disk had more than one
    // tenant on it.
    let incoming_bytes: u64 = incoming.files.iter().map(|(_, b)| b.len() as u64).sum();
    let held = app.files.user_bytes(&user.id);
    if user.quota_bytes > 0 && held + incoming_bytes > user.quota_bytes as u64 {
        return Failure::json(
            StatusCode::INSUFFICIENT_STORAGE,
            format!(
                "this would put you at {} bytes against a quota of {}. Published \
                 versions are never deleted, so the way down is a smaller bundle \
                 or a larger quota.",
                held + incoming_bytes,
                user.quota_bytes
            ),
        )
        .into_response();
    }

    if let Err(e) = app.files.install(&user.id, &id, version, &incoming.files) {
        tracing::error!(%id, version, error = %e, "installing a version");
        return Failure::json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not store the bundle",
        )
        .into_response();
    }

    let row = crate::db::BundleRow {
        user_id: user.id.clone(),
        id: id.clone(),
        version,
        digest: incoming.digest.clone(),
        class: incoming.manifest.class,
        title: incoming.manifest.title.clone(),
        description: incoming.manifest.description.clone(),
        template: incoming.manifest.template.clone(),
        published_at: incoming.manifest.published_at.clone(),
        received_at: crate::db::now(),
        withheld_at: None,
        withheld_reason: None,
    };
    if let Err(e) = app.db.bundle_insert(&row) {
        tracing::error!(%id, version, error = %e, "indexing a version");
        return Failure::json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not index the bundle",
        )
        .into_response();
    }
    if let Some(key) = &idempotency {
        let _ = app
            .db
            .idempotency_record(&user.id, key, &id, version, &crate::db::now());
    }
    tracing::info!(handle = %user.handle, %id, version, digest = %incoming.digest, "published");
    published(&app, &user, &id, version, false, StatusCode::CREATED)
}

/// What a publish answers with: enough to print a URL without asking again.
fn published(
    app: &Shared,
    user: &UserRow,
    id: &str,
    version: u32,
    existing: bool,
    status: StatusCode,
) -> Response {
    let class = app
        .db
        .bundle(&user.id, id, version)
        .ok()
        .flatten()
        .map(|row| row.class)
        .unwrap_or_default();
    let base = app.config.user_url(Role::for_class(class), &user.handle);
    (
        status,
        Json(serde_json::json!({
            "id": id,
            "version": version,
            "existing": existing,
            "class": class.as_str(),
            "url": format!("{base}/b/{id}/"),
            "version_url": format!("{base}/b/{id}/v/{version}/"),
        })),
    )
        .into_response()
}

#[derive(serde::Deserialize)]
pub struct AliasRequest {
    /// `null` takes it down. The versions stay; nothing points at them.
    #[serde(default)]
    pub version: Option<u32>,
    /// Omitted leaves it as it was — so "move the alias" and "make it public"
    /// are separate acts, and neither does the other by accident.
    #[serde(default)]
    pub visibility: Option<String>,
}

/// `POST /v1/bundles/{id}/alias` — point the share URL at a version.
///
/// This is the verb that publishes, in the sense a reader cares about: until an
/// alias names a version and the visibility says public, the origin serves
/// nothing. Which is why mecha routes it through the outbox exactly like a
/// publish rather than treating it as bookkeeping.
pub async fn alias(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    let user = match authorised(&app, &headers, Scope::Publish) {
        Ok((_, user)) => user,
        Err(refusal) => return *refusal,
    };
    if mecha_manifest::valid_id(&id, "bundle id").is_err() {
        return Failure::json(StatusCode::BAD_REQUEST, "not a bundle id").into_response();
    }
    let request: AliasRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(e) => {
            return Failure::json(StatusCode::BAD_REQUEST, format!("body: {e}")).into_response()
        }
    };

    if let Some(version) = request.version {
        match app.db.bundle(&user.id, &id, version) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Failure::json(
                    StatusCode::NOT_FOUND,
                    format!("{id} has no version {version}"),
                )
                .into_response()
            }
            Err(e) => {
                tracing::error!(%id, error = %e, "reading the ledger");
                return Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                    .into_response();
            }
        }
    }

    let current = app.db.alias(&user.id, &id).ok().flatten();
    let visibility = match request.visibility.as_deref() {
        Some("public") => Visibility::Public,
        Some("private") => Visibility::Private,
        None => current.map(|a| a.visibility).unwrap_or_default(),
        Some(other) => {
            return Failure::json(
                StatusCode::BAD_REQUEST,
                format!("visibility `{other}` is not public or private"),
            )
            .into_response()
        }
    };

    if let Err(e) = app.db.alias_set(
        &user.id,
        &id,
        request.version,
        visibility,
        &crate::db::now(),
    ) {
        tracing::error!(%id, error = %e, "moving an alias");
        return Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
    }
    tracing::info!(%id, version = ?request.version, visibility = ?visibility, "alias moved");

    let class = request
        .version
        .and_then(|v| app.db.bundle(&user.id, &id, v).ok().flatten())
        .map(|row| row.class)
        .unwrap_or_default();
    let base = app.config.user_url(Role::for_class(class), &user.handle);
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "id": id,
            "version": request.version,
            "visibility": match visibility {
                Visibility::Public => "public",
                Visibility::Private => "private",
            },
            "url": format!("{base}/b/{id}/"),
        })),
    )
        .into_response()
}

/// `PUT /v1/types/{id}` — upload a request-type manifest.
///
/// The body is the TOML, and **the server generates the JSON Schema itself**
/// rather than accepting one. Two artifacts that must agree are two artifacts
/// that eventually do not; deriving the schema from the manifest here means the
/// form a stranger fills, the schema an agent discovers, and the validation the
/// edge runs all come from the one file — which is the whole premise of the
/// manifest crate.
pub async fn put_type(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    let user = match authorised(&app, &headers, Scope::Publish) {
        Ok((_, user)) => user,
        Err(refusal) => return *refusal,
    };
    let parsed = match mecha_manifest::RequestType::from_toml(&body) {
        Ok(parsed) => parsed,
        Err(e) => {
            return Failure::json(StatusCode::BAD_REQUEST, format!("manifest: {e}")).into_response()
        }
    };
    if parsed.id != id {
        return Failure::json(
            StatusCode::BAD_REQUEST,
            format!(
                "the manifest calls itself `{}` and the URL says `{id}` — \
                 the id is a path segment, a filename and a tool name, so it \
                 cannot be two things",
                parsed.id
            ),
        )
        .into_response();
    }

    let row = crate::db::TypeRow {
        user_id: user.id.clone(),
        id: parsed.id.clone(),
        title: parsed.title.clone(),
        manifest: body,
        schema: parsed.json_schema().to_string(),
        updated_at: crate::db::now(),
    };
    if let Err(e) = app.db.type_put(&row) {
        tracing::error!(%id, error = %e, "storing a type");
        return Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
    }
    tracing::info!(%id, fields = parsed.fields.len(), "type uploaded");
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "id": row.id,
            "title": row.title,
            "fields": parsed.fields.len(),
            "url": format!("{}/v1/types/{}", app.config.base_url(Role::Gate), row.id),
        })),
    )
        .into_response()
}

/// `GET /v1/types` — what this surface accepts.
///
/// Public, because discovery is the point: an agent that finds this endpoint
/// can learn the shape of every request it could make without being told
/// anything first. A request type is our own declaration, not anyone's data.
pub async fn list_types(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    // Discovery is per-user, and therefore authenticated: "what can this
    // surface accept" has a different answer for each person on it, and there
    // is no answer for somebody who is not on it at all.
    let user = match authorised(&app, &headers, Scope::Publish) {
        Ok((_, user)) => user,
        Err(refusal) => return *refusal,
    };
    match app.db.types(&user.id) {
        Ok(rows) => {
            let base = app.config.base_url(Role::Gate);
            let types: Vec<_> = rows
                .iter()
                .map(|row| {
                    serde_json::json!({
                        "id": row.id,
                        "title": row.title,
                        "updated_at": row.updated_at,
                        "url": format!("{base}/v1/types/{}", row.id),
                    })
                })
                .collect();
            (StatusCode::OK, Json(serde_json::json!({ "types": types }))).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "listing types");
            Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

/// `GET /v1/types/{id}` — one type's schema, and its manifest.
pub async fn get_type(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    let user = match authorised(&app, &headers, Scope::Publish) {
        Ok((_, user)) => user,
        Err(refusal) => return *refusal,
    };
    match app.db.type_get(&user.id, &id) {
        Ok(Some(row)) => {
            let schema: serde_json::Value =
                serde_json::from_str(&row.schema).unwrap_or(serde_json::Value::Null);
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": row.id,
                    "title": row.title,
                    "updated_at": row.updated_at,
                    "schema": schema,
                    "manifest": row.manifest,
                })),
            )
                .into_response()
        }
        Ok(None) => Failure::json(StatusCode::NOT_FOUND, "no such type").into_response(),
        Err(e) => {
            tracing::error!(%id, error = %e, "reading a type");
            Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
pub struct DrainQuery {
    /// The highest sequence number home already holds.
    #[serde(default)]
    pub since: i64,
}

/// `GET /v1/queue?since={seq}` — take what has been verified and not yet
/// acknowledged.
///
/// A pure read (see `Db::drain`). The records are not marked, so a response
/// that never arrives costs nothing but a repeat — and repeating is correct,
/// because the alternative is a stranger's request disappearing into a dropped
/// connection.
pub async fn drain(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Query(query): Query<DrainQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    let user = match authorised(&app, &headers, Scope::Drain) {
        Ok((_, user)) => user,
        Err(refusal) => return *refusal,
    };
    match app
        .db
        .drain(&user.id, query.since, app.config.limits.drain_batch)
    {
        Ok(rows) => {
            let records: Vec<_> = rows
                .iter()
                .map(|row| {
                    serde_json::json!({
                        "seq": row.seq,
                        "type": row.type_id,
                        "state": row.state,
                        "created_at": row.created_at,
                        // Passed through as text rather than as parsed JSON:
                        // the server validated its shape on the way in, and
                        // re-serialising a stranger's record here would be this
                        // box deciding what home reads. Home parses and
                        // re-validates against the schema it uploaded.
                        "payload": row.payload,
                    })
                })
                .collect();
            let next = rows.iter().map(|r| r.seq).max().unwrap_or(query.since);
            tracing::info!(count = records.len(), since = query.since, "drained");
            (
                StatusCode::OK,
                Json(serde_json::json!({ "records": records, "next": next })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "draining");
            Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
pub struct AckRequest {
    /// Exactly what home has stored. Not a watermark: a watermark deletes rows
    /// nobody named, and the failure is silent.
    pub seqs: Vec<i64>,
}

/// `POST /v1/queue/ack` — delete what home says it has.
///
/// The only destructive operation on the box, which is why it is the one that
/// names its subjects one by one.
pub async fn ack(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    let user = match authorised(&app, &headers, Scope::Drain) {
        Ok((_, user)) => user,
        Err(refusal) => return *refusal,
    };
    let request: AckRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(e) => {
            return Failure::json(StatusCode::BAD_REQUEST, format!("body: {e}")).into_response()
        }
    };
    match app.db.queue_ack(&user.id, &request.seqs) {
        Ok(deleted) => {
            tracing::info!(deleted, asked = request.seqs.len(), "acknowledged");
            (
                StatusCode::OK,
                Json(serde_json::json!({ "deleted": deleted })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "acknowledging");
            Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}
