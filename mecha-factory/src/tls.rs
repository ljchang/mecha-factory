//! The certificate, obtained by the binary itself.
//!
//! §13.2 settled this: no CDN and no reverse proxy in front, because anything
//! that terminates TLS reads the plaintext of every request and response — the
//! drained submissions included — and "control our own infrastructure" is
//! mostly a claim about exactly that. So the program does its own ACME, and the
//! private key exists in one process on one box.
//!
//! **HTTP-01, not TLS-ALPN-01 and not DNS-01.** This changed when a second
//! person had to be able to sign up without an operator; [`crate::certificates`]
//! carries the argument in full. What it means here:
//!
//! - **Port 80 is part of issuance**, which it deliberately was not before. It
//!   still serves the redirect a human typing a bare hostname needs, and now
//!   also `/.well-known/acme-challenge/{token}`. `[listen] http` is therefore
//!   no longer optional beside a `[tls]` block, and `Config::check` refuses the
//!   combination — a box that comes up serving TLS and quietly cannot renew is
//!   the silently-degrading shape this project keeps naming.
//! - **The challenge is answered on 80 directly, never redirected.** Let's
//!   Encrypt does follow redirects, but the HTTPS side has no valid certificate
//!   for a name being issued for the first time, which is the only case that
//!   matters here.
//! - No DNS credential lives on the box. DNS-01 would put an API token for the
//!   whole zone on the machine we have agreed to assume is lost, and it buys
//!   nothing until wildcards do — which `rustls-acme` forecloses anyway, since
//!   `UseChallenge` is `Http01 | TlsAlpn01`.
//! - **Nothing may terminate TLS in front of this.** Still true, and now for a
//!   second reason: a proxy on 80 answers the challenge as well.
//!
//! The account key and the certificates are cached in the data directory, so a
//! restart re-issues nothing. `staging = true` points at the directory whose
//! certificates nobody trusts and whose limits are large, which is how you find
//! out the path works without spending a week's budget discovering it does not.

use anyhow::{Context, Result};
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::certificates::{self, Acme, Registry};
use crate::config::Config;
use crate::http::{router, App};

/// How often the ledger is re-read for handles that have no certificate.
///
/// A person who has just signed up is waiting on this plus an ACME round trip,
/// so it is short — and it costs one indexed `SELECT` against a local SQLite
/// file, which is cheaper than the timer that schedules it.
const RECONCILE_EVERY: Duration = Duration::from_secs(30);

/// Serve HTTPS on `listen.https`, with certificates this process obtains.
pub async fn serve(app: Arc<App>, config: &Config) -> Result<()> {
    let acme = Acme::from_config(config).context("serve_tls called without a [tls] block")?;
    let staging = acme.staging;
    let http = config.listen.http.context(
        "a [tls] block needs [listen] http: certificates are issued over HTTP-01, \
         which is answered on port 80",
    )?;
    std::fs::create_dir_all(config.acme_cache())?;

    let registry = Registry::new(acme.clone());
    registry
        .seed_account()
        .await
        .context("seeding the acme account key")?;

    // Opportunistic on purpose: a migration that cannot read the ledger has
    // nothing to migrate for, and the cost of skipping is the cost the upgrade
    // always had — fresh orders — not an outage.
    match app.db.users() {
        Ok(users) => {
            if let Err(e) = certificates::migrate_combined_cache(&acme, config, &users).await {
                tracing::warn!(error = %e, "migrating the pre-upgrade certificate cache");
            }
        }
        Err(e) => tracing::warn!(error = %e, "reading the ledger to migrate the certificate cache"),
    }

    // **The challenge listener is bound before the first order is started.**
    // On a cold cache an `AcmeState` begins ordering the moment it is spawned,
    // and a validation GET that arrives before port 80 answers is a failed
    // authorization plus hours of internal backoff — for an order that would
    // have succeeded moments later. Binding is also the one failure worth
    // dying on the spot for: this port is where certificates come from now,
    // and a server that came up without it would serve its cache for sixty
    // days and then stop.
    let challenge_listener = tokio::net::TcpListener::bind(http)
        .await
        .with_context(|| format!("binding {http} — port 80 answers the acme challenges"))?;
    tracing::info!(address = %http, "serving acme challenges and redirecting to https");

    // The first pass, before anything is served. An error is a warning and not
    // an exit: the base group was already ensured before the ledger read that
    // can fail, and the reconcile loop retries in thirty seconds — where
    // exiting would be a total outage grown from a transiently locked SQLite
    // file.
    match certificates::reconcile(&registry, &app.db, config) {
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "the first reconcile pass failed; retrying in the loop")
        }
    }
    tracing::info!(
        groups = registry.groups(),
        "ordering certificates over http-01; a user created from here on gets one without a restart"
    );
    if staging {
        tracing::warn!(
            "using the Let's Encrypt staging directory — browsers will refuse \
             these certificates"
        );
    }
    tracing::info!(address = %config.listen.https, "serving tls");

    // ALPN is deliberately left unset, which is what the acceptor this replaced
    // did too: the server advertises nothing and clients arrive over HTTP/1.1.
    // Turning on h2 is a change with its own consequences and is not this one.
    let acceptor = RustlsAcceptor::new(RustlsConfig::from_config(registry.server_config()));
    let service = router(app.clone()).into_make_service_with_connect_info::<SocketAddr>();
    let https = tokio::spawn(
        axum_server::bind(config.listen.https)
            .acceptor(acceptor)
            .serve(service),
    );

    let port80 = {
        let app = app.clone();
        let registry = registry.clone();
        tokio::spawn(async move { axum::serve(challenge_listener, port_80(app, registry)).await })
    };
    let reconcile = tokio::spawn(reconcile_forever(app.clone(), registry));

    // Either listener failing ends `serve`, loudly. The port-80 handle used to
    // be spawned and dropped, which made its death invisible — and an
    // invisible port 80 is certificates that stop renewing with no log line,
    // which is precisely the shape `Config::check`'s refusal exists to
    // prevent. A select rather than a join: the survivor is aborted, because
    // half a server is not a state to keep running in.
    let result = tokio::select! {
        https = https => https.context("the tls listener panicked")?.map_err(Into::into),
        port80 = port80 => match port80.context("the challenge listener panicked")? {
            Ok(()) => Err(anyhow::anyhow!("the port-80 listener stopped")),
            Err(e) => Err(e.into()),
        },
    };
    reconcile.abort();
    result
}

/// Re-read the ledger, and order for whatever is in it that we do not have.
///
/// **This is why a signup needs no restart**, and why it needs no notification
/// either: `factory user create` runs in another process, so a channel would
/// only ever fire for the endpoint that does not exist yet. A failure is
/// logged and the next pass retries — the ledger is the truth, so a pass that
/// could not read it has lost nothing.
async fn reconcile_forever(app: Arc<App>, registry: Arc<Registry>) {
    let mut ticker = tokio::time::interval(RECONCILE_EVERY);
    // The first tick is immediate and `serve` has already reconciled.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        match certificates::reconcile(&registry, &app.db, &app.config) {
            Ok(0) => {}
            Ok(started) => tracing::info!(
                started,
                groups = registry.groups(),
                "a new handle appeared; ordering its certificate"
            ),
            Err(e) => tracing::warn!(error = %e, "reading the ledger to reconcile certificates"),
        }
    }
}

/// Port 80: the ACME challenge, and one redirect.
///
/// The challenge route is registered ahead of the redirect, and that ordering
/// is the whole of it — a challenge redirected to a name with no certificate
/// yet is an order that never completes. Let's Encrypt does follow
/// redirects, which is exactly why this is easy to get wrong and impossible to
/// notice: it would keep working for every name that already has a certificate
/// and fail only for the ones being issued for the first time, which is every
/// signup.
///
/// The redirect answers only for names we serve. Redirecting an unknown `Host`
/// would make this an open redirector — a small thing on its own and a real one
/// when the same origin later hands out capability URLs.
///
/// Built here rather than inline so a test can drive it: see
/// `tests/certificates.rs`.
pub fn port_80(app: Arc<App>, registry: Arc<Registry>) -> axum::Router {
    use axum::extract::{Path, State};
    use axum::http::{header, HeaderMap, StatusCode, Uri};
    use axum::response::{IntoResponse, Redirect, Response};

    /// The key authorization for a token some order is waiting on.
    ///
    /// Deliberately not host-scoped: the token is the secret, it is
    /// per-authorization, and a resolver that does not know it says nothing. A
    /// `Host` check here would only add a way for a validation from an
    /// unexpected vantage point to fail.
    async fn challenge(
        State(registry): State<Arc<Registry>>,
        Path(token): Path<String>,
    ) -> Response {
        match registry.http_01_key_auth(&token) {
            Some(key_auth) => (
                [(header::CONTENT_TYPE, "application/octet-stream")],
                key_auth,
            )
                .into_response(),
            None => {
                tracing::debug!(%token, "an acme challenge nobody is waiting on");
                (StatusCode::NOT_FOUND, "not found").into_response()
            }
        }
    }

    // The `Host` extractor left axum in 0.8; the header is what it read anyway,
    // and reading it here keeps this resolving names exactly the way the real
    // router does.
    async fn handler(State(app): State<Arc<App>>, headers: HeaderMap, uri: Uri) -> Response {
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        match app.config.origins.role_of(host) {
            Some(origin) => {
                // Redirect to the name they asked for, not to the bare origin:
                // `alice.artifacts.example.org` on port 80 must arrive at
                // `https://alice.artifacts.example.org`, or every user's plain
                // links would land on somebody else's namespace.
                let scheme = if app.config.tls.is_some() {
                    "https"
                } else {
                    "http"
                };
                let base = match &origin.handle {
                    Some(handle) => app.config.origins.host_for(origin.role, handle),
                    None => app.config.origins.authority(origin.role).to_string(),
                };
                let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
                Redirect::permanent(&format!("{scheme}://{base}{path}")).into_response()
            }
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        }
    }

    axum::Router::new()
        .route(
            "/.well-known/acme-challenge/{token}",
            axum::routing::get(challenge),
        )
        .with_state(registry)
        .merge(
            axum::Router::new()
                .fallback(axum::routing::any(handler))
                .with_state(app),
        )
}

/// What `factory check` prints about the certificate situation, without
/// touching the network.
pub fn describe(config: &Config) -> String {
    match &config.tls {
        None => "none (loopback only)".to_string(),
        Some(tls) => {
            let cached = std::fs::read_dir(config.acme_cache())
                .map(|entries| entries.flatten().count())
                .unwrap_or(0);
            format!(
                "acme http-01 for {} plus one certificate per active user \
                 ({}{} cached files in {}), contact {}",
                config.origins.names().join(", "),
                if tls.staging { "staging, " } else { "" },
                cached,
                config.acme_cache().display(),
                tls.contact
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `check` names the challenge type, because it is the sentence somebody
    /// reads before deciding whether port 80 may be firewalled off.
    #[test]
    fn the_staging_directory_and_the_challenge_type_are_named_in_what_check_prints() {
        let staging = describe(&Config::example());
        assert!(staging.contains("staging"), "{staging}");
        assert!(staging.contains("http-01"), "{staging}");
        assert!(staging.contains("per active user"), "{staging}");

        let mut loopback = Config::example();
        loopback.tls = None;
        assert!(describe(&loopback).contains("loopback"));
    }
}
