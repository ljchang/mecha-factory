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
    let user = match authorised(&app, &headers, Scope::Release) {
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
    let user = match authorised(&app, &headers, Scope::Release) {
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

/// What `factory-publish connect` sends.
#[derive(serde::Deserialize)]
pub struct PairRequest {
    pub code: String,
    /// The handle the person running `connect` expects this machine to
    /// publish for. **Checked by the server**, which is what makes the
    /// reversed-device-code defence structural: no client can skip the
    /// assertion, because there is no redemption without it.
    pub handle: String,
    /// Free text for `key list` — the client sends its hostname.
    #[serde(default)]
    pub label: String,
}

/// `POST /v1/pair` — spend a pairing code, receive this machine's keys.
///
/// Unauthenticated: the code is the credential, single-use and minutes-lived.
/// Every refusal is the same refusal. A wrong handle assertion in particular
/// spends nothing and reveals nothing — it must not be the probe that tells
/// whoever holds a forwarded code which account it belongs to.
pub async fn pair(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    let request: PairRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(e) => {
            return Failure::json(StatusCode::BAD_REQUEST, format!("body: {e}")).into_response()
        }
    };
    let asserted = request.handle.trim().to_ascii_lowercase();
    // The label rides into `key list` output, so it is bounded and stripped
    // of anything that is not printable — it is the one free-text field on
    // this endpoint.
    let label: String = request
        .label
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();

    // Hashing before the transaction: compute outside, decide inside.
    let prepared = keys::prepare("", Scope::Publish, &label)
        .and_then(|publish| Ok((publish, keys::prepare("", Scope::Drain, &label)?)));
    let (publish, drain) = match prepared {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(error = %e, "preparing keys for a pairing");
            return Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    };

    let hash = crate::intake::hash_token(request.code.trim());
    match app.db.pairing_redeem(
        &hash,
        &asserted,
        &publish.row,
        &drain.row,
        &crate::db::now(),
    ) {
        Ok(crate::db::Paired::Redeemed(user)) => {
            tracing::info!(handle = %user.handle, %label, "a machine paired");
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "handle": user.handle,
                    "publish_key": publish.token,
                    "publish_key_id": publish.row.id,
                    "drain_key": drain.token,
                    "drain_key_id": drain.row.id,
                    "artifacts_url": app.config.user_url(Role::Artifacts, &user.handle),
                    "compute_url": app.config.user_url(Role::Compute, &user.handle),
                })),
            )
                .into_response()
        }
        Ok(crate::db::Paired::Refused) => Failure::json(
            StatusCode::NOT_FOUND,
            "that code is not valid for that handle",
        )
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "redeeming a pairing code");
            Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

/// `POST /v1/disconnect` — the presented key revokes itself.
///
/// A credential may always retire itself: no scope check, because whichever
/// scope the key has is exactly the authority being surrendered — and no
/// body, because the Bearer token already names the one key this can touch.
/// This is what makes a compromised laptop recoverable by the person who
/// owns it, from the laptop, rather than only by whoever holds root on the
/// box.
pub async fn disconnect(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    let row = match keys::authenticate_any(&app.db, bearer(&headers)) {
        Ok(row) => row,
        Err(e) => {
            tracing::warn!(error = %e, "refused");
            let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::UNAUTHORIZED);
            return Failure::json(status, e.public_message()).into_response();
        }
    };
    match app.db.key_revoke(&row.id, &crate::db::now()) {
        Ok(_) => {
            tracing::info!(key = %row.id, scope = row.scope.as_str(), "a key retired itself");
            (
                StatusCode::OK,
                Json(serde_json::json!({ "revoked": row.id })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "revoking a key from itself");
            Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

// ---- the operator's endpoints ------------------------------------------
//
// What retires the SSH session from routine operation. All of it sits behind
// `Scope::Operate` — a credential bound to the box rather than to a tenant —
// and none of it is reachable by any tenant key: the tenant authoriser joins
// on a user and an operate key has none, and this authoriser demands the one
// scope no tenant key carries. The two surfaces the design doc said must not
// be one, kept apart by the credential rather than by attention.

/// The operator, or a refusal. No user join: the key belongs to the box.
fn authorised_operator(app: &Shared, headers: &HeaderMap) -> Result<KeyRow, Box<Response>> {
    match keys::authenticate(&app.db, bearer(headers), Scope::Operate) {
        Ok(row) => Ok(row),
        Err(e) => {
            tracing::warn!(error = %e, "refused (operator)");
            let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::UNAUTHORIZED);
            Err(Box::new(
                Failure::json(status, e.public_message()).into_response(),
            ))
        }
    }
}

/// `GET /v1/admin/users` — everyone, with their queue depth beside them.
pub async fn admin_users(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    if let Err(refusal) = authorised_operator(&app, &headers) {
        return *refusal;
    }
    let users = match app.db.users() {
        Ok(users) => users,
        Err(e) => {
            tracing::error!(error = %e, "listing users");
            return Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    };
    let rows: Vec<serde_json::Value> = users
        .iter()
        .map(|user| {
            serde_json::json!({
                "handle": user.handle,
                "email": user.email,
                "status": user.status,
                "created_at": user.created_at,
                "quota_bytes": user.quota_bytes,
                "queued": app.db.queue_depth(Some(&user.id)).unwrap_or(0),
            })
        })
        .collect();
    (StatusCode::OK, Json(serde_json::json!({ "users": rows }))).into_response()
}

#[derive(serde::Deserialize)]
pub struct AdminStatusRequest {
    pub status: String,
}

/// `POST /v1/admin/users/{handle}/status` — suspend or restore.
pub async fn admin_user_status(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path(handle): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    if let Err(refusal) = authorised_operator(&app, &headers) {
        return *refusal;
    }
    let request: AdminStatusRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(e) => {
            return Failure::json(StatusCode::BAD_REQUEST, format!("body: {e}")).into_response()
        }
    };
    if !matches!(request.status.as_str(), "active" | "suspended") {
        return Failure::json(StatusCode::BAD_REQUEST, "status is active or suspended")
            .into_response();
    }
    let user = match app.db.user_by_handle(&handle) {
        Ok(Some(user)) => user,
        Ok(None) => return Failure::json(StatusCode::NOT_FOUND, "no such handle").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "reading a user");
            return Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    };
    if let Err(e) = app.db.user_status(&user.id, &request.status) {
        tracing::error!(error = %e, "setting a status");
        return Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
    }
    tracing::info!(%handle, status = %request.status, "operator set a status");
    (
        StatusCode::OK,
        Json(serde_json::json!({ "handle": handle, "status": request.status })),
    )
        .into_response()
}

#[derive(serde::Deserialize)]
pub struct AdminInviteRequest {
    pub email: String,
    #[serde(default)]
    pub note: String,
}

/// `POST /v1/admin/invites` — mint an invite; **the box mails it**, which is
/// better than the on-box CLI could do for a remote operator: the link never
/// travels anywhere but to its recipient and back in this response.
pub async fn admin_invite_create(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    if let Err(refusal) = authorised_operator(&app, &headers) {
        return *refusal;
    }
    let request: AdminInviteRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(e) => {
            return Failure::json(StatusCode::BAD_REQUEST, format!("body: {e}")).into_response()
        }
    };
    let email = request.email.trim().to_string();
    if email.is_empty() || !email.contains('@') {
        return Failure::json(StatusCode::BAD_REQUEST, "an email address is required")
            .into_response();
    }
    let token = crate::intake::mint_token();
    let expires = (chrono::Utc::now() + chrono::Duration::days(crate::intake::INVITE_EXPIRY_DAYS))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let row = match app.db.invite_create(
        &email,
        &request.note,
        &crate::intake::hash_token(&token),
        &crate::db::now(),
        &expires,
    ) {
        Ok(row) => row,
        Err(e) => {
            tracing::error!(error = %e, "minting an invite");
            return Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    };
    let link = format!("{}/signup/{token}", app.config.base_url(Role::Gate));
    app.mailer.send_invite(&email, &link);
    tracing::info!(invite = %row.id, "operator minted an invite");
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "id": row.id,
            "link": link,
            "expires_at": expires,
            "mailed_via": app.mailer.describe(),
        })),
    )
        .into_response()
}

/// `GET /v1/admin/invites` — every invite and what became of it.
pub async fn admin_invites(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    if let Err(refusal) = authorised_operator(&app, &headers) {
        return *refusal;
    }
    let now = crate::db::now();
    match app.db.invites() {
        Ok(invites) => {
            let rows: Vec<serde_json::Value> = invites
                .iter()
                .map(|row| {
                    serde_json::json!({
                        "id": row.id,
                        "email": row.email,
                        "note": row.note,
                        "status": row.status(&now),
                        "created_at": row.created_at,
                        "expires_at": row.expires_at,
                        "claimed_by": row.claimed_by,
                    })
                })
                .collect();
            (StatusCode::OK, Json(serde_json::json!({ "invites": rows }))).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "listing invites");
            Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

/// `POST /v1/admin/invites/{id}/revoke`.
pub async fn admin_invite_revoke(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    if let Err(refusal) = authorised_operator(&app, &headers) {
        return *refusal;
    }
    match app.db.invite_revoke(&id, &crate::db::now()) {
        Ok(revoked) => (
            StatusCode::OK,
            Json(serde_json::json!({ "revoked": revoked })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "revoking an invite");
            Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

/// `GET /v1/admin/keys` — every key, live and dead, with whose it is.
pub async fn admin_keys(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    if let Err(refusal) = authorised_operator(&app, &headers) {
        return *refusal;
    }
    let users = app.db.users().unwrap_or_default();
    match app.db.keys() {
        Ok(keys) => {
            let rows: Vec<serde_json::Value> = keys
                .iter()
                .map(|key| {
                    let handle = users
                        .iter()
                        .find(|u| u.id == key.user_id)
                        .map(|u| u.handle.as_str())
                        .unwrap_or(if key.user_id.is_empty() {
                            "(operator)"
                        } else {
                            "(unknown)"
                        });
                    serde_json::json!({
                        "id": key.id,
                        "handle": handle,
                        "scope": key.scope.as_str(),
                        "label": key.label,
                        "created_at": key.created_at,
                        "revoked_at": key.revoked_at,
                        "last_used_at": key.last_used_at,
                    })
                })
                .collect();
            (StatusCode::OK, Json(serde_json::json!({ "keys": rows }))).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "listing keys");
            Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

/// `POST /v1/admin/keys/{id}/revoke` — break-glass, any key, including
/// another operate key. The row stays, as everywhere.
pub async fn admin_key_revoke(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    if let Err(refusal) = authorised_operator(&app, &headers) {
        return *refusal;
    }
    match app.db.key_revoke(&id, &crate::db::now()) {
        Ok(revoked) => {
            if revoked {
                tracing::info!(key = %id, "operator revoked a key");
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({ "revoked": revoked })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "revoking a key");
            Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
pub struct AdminWithholdRequest {
    pub handle: String,
    pub id: String,
    pub version: u32,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub undo: bool,
}

/// `POST /v1/admin/withhold` — take a version out of service, reversibly.
pub async fn admin_withhold(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = not_on_gate(&origin) {
        return refusal;
    }
    if let Err(refusal) = authorised_operator(&app, &headers) {
        return *refusal;
    }
    let request: AdminWithholdRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(e) => {
            return Failure::json(StatusCode::BAD_REQUEST, format!("body: {e}")).into_response()
        }
    };
    let user = match app.db.user_by_handle(&request.handle) {
        Ok(Some(user)) => user,
        Ok(None) => return Failure::json(StatusCode::NOT_FOUND, "no such handle").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "reading a user");
            return Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    };
    let now = crate::db::now();
    let changed = app.db.bundle_withhold(
        &user.id,
        &request.id,
        request.version,
        if request.undo {
            None
        } else {
            request.reason.as_deref()
        },
        if request.undo { None } else { Some(&now) },
    );
    match changed {
        Ok(true) => {
            tracing::info!(handle = %request.handle, id = %request.id, version = request.version,
                           undo = request.undo, "operator changed a withhold");
            (StatusCode::OK, Json(serde_json::json!({ "changed": true }))).into_response()
        }
        Ok(false) => Failure::json(StatusCode::NOT_FOUND, "no such version").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "withholding");
            Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}
