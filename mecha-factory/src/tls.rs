//! The certificate, obtained by the binary itself.
//!
//! §13.2 settled this: no CDN and no reverse proxy in front, because anything
//! that terminates TLS reads the plaintext of every request and response — the
//! drained submissions included — and "control our own infrastructure" is
//! mostly a claim about exactly that. So the program does its own ACME, and the
//! private key exists in one process on one box.
//!
//! **TLS-ALPN-01, not HTTP-01 and not DNS-01.** The challenge is answered
//! inside the TLS handshake on 443, which means:
//!
//! - Port 80 is not part of issuance. It exists here only because people type
//!   bare hostnames, and it serves one redirect and nothing else.
//! - No DNS credential lives on the box. DNS-01 would put an API token for the
//!   whole zone on the machine we have agreed to assume is lost, and it buys
//!   nothing until per-bundle wildcard subdomains exist (§7.6).
//! - **Nothing may terminate TLS in front of this.** A proxy that answers the
//!   handshake answers the challenge, and issuance stops working. That is worth
//!   knowing before anybody turns on Cloudflare's orange cloud: proxied means
//!   moving to DNS-01 *and* handing the plaintext to somebody else.
//!
//! The account key and the certificates are cached in the data directory, so a
//! restart does not re-issue. Let's Encrypt's rate limits are per-week and
//! generous but not infinite; `staging = true` points at the directory whose
//! certificates nobody trusts and whose limits are large, which is how you find
//! out the path works without spending a week's budget discovering it does not.

use anyhow::{Context, Result};
use futures_util::StreamExt;
use rustls_acme::caches::DirCache;
use rustls_acme::AcmeConfig;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::config::Config;
use crate::http::{router, App};

/// Serve HTTPS on `listen.https`, with certificates this process obtains.
pub async fn serve(app: Arc<App>, config: &Config) -> Result<()> {
    let tls = config
        .tls
        .as_ref()
        .context("serve_tls called without a [tls] block")?;
    let names = config.origins.names();
    std::fs::create_dir_all(config.acme_cache())?;

    let mut state = AcmeConfig::new(names.clone())
        .contact([tls.contact.clone()])
        .cache(DirCache::new(config.acme_cache()))
        .directory_lets_encrypt(!tls.staging)
        .state();
    let acceptor = state.axum_acceptor(state.default_rustls_config());

    // The ACME state machine is a stream that has to be polled for anything to
    // be ordered or renewed. Its events are logged rather than swallowed: a
    // certificate that silently failed to renew is a site that goes down in
    // sixty days for a reason nobody wrote down.
    tokio::spawn(async move {
        loop {
            match state.next().await {
                Some(Ok(event)) => tracing::info!(?event, "acme"),
                Some(Err(e)) => tracing::error!(error = %e, "acme"),
                None => {
                    tracing::error!("the acme state machine ended; certificates will not renew");
                    break;
                }
            }
        }
    });

    if tls.staging {
        tracing::warn!(
            "using the Let's Encrypt staging directory — browsers will refuse \
             these certificates"
        );
    }
    tracing::info!(names = ?names, address = %config.listen.https, "serving tls");

    let service = router(app.clone()).into_make_service_with_connect_info::<SocketAddr>();
    let https = tokio::spawn(
        axum_server::bind(config.listen.https)
            .acceptor(acceptor)
            .serve(service),
    );

    let redirect = config.listen.http.map(|address| {
        let app = app.clone();
        tokio::spawn(async move { redirect_to_https(app, address).await })
    });

    https.await.context("the tls listener panicked")??;
    if let Some(redirect) = redirect {
        redirect.abort();
    }
    Ok(())
}

/// Port 80: one redirect, and nothing else.
///
/// It answers only for names we serve. Redirecting an unknown `Host` would make
/// this an open redirector — a small thing on its own and a real one when the
/// same origin later hands out capability URLs.
async fn redirect_to_https(app: Arc<App>, address: SocketAddr) -> Result<()> {
    use axum::extract::State;
    use axum::http::{header, HeaderMap, StatusCode, Uri};
    use axum::response::{IntoResponse, Redirect, Response};

    // The `Host` extractor left axum in 0.8; the header is what it read anyway,
    // and reading it here keeps this resolving names exactly the way the real
    // router does.
    async fn handler(State(app): State<Arc<App>>, headers: HeaderMap, uri: Uri) -> Response {
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        match app.config.origins.role_of(host) {
            Some(role) => {
                let base = app.config.base_url(role);
                let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
                Redirect::permanent(&format!("{base}{path}")).into_response()
            }
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        }
    }

    let router = axum::Router::new()
        .fallback(axum::routing::any(handler))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("binding {address}"))?;
    tracing::info!(%address, "redirecting to https");
    axum::serve(listener, router).await?;
    Ok(())
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
                "acme tls-alpn-01 for {} ({}{} cached files in {}), contact {}",
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
    use crate::config::{Limits, Listen, Origins, Tls};
    use std::path::PathBuf;

    fn config(tls: Option<Tls>) -> Config {
        Config {
            data_dir: PathBuf::from("/tmp/factory-test"),
            origins: Origins {
                gate: "gate.example.org".into(),
                artifacts: "art.example.org".into(),
                compute: "compute.example.org".into(),
            },
            listen: Listen {
                https: ([0, 0, 0, 0], 443).into(),
                http: None,
            },
            tls,
            limits: Limits::default(),
        }
    }

    /// The certificate covers exactly the three names, or one of the origins
    /// is unreachable in a way that only shows up in a browser.
    #[test]
    fn the_order_covers_every_origin_and_nothing_else() {
        let config = config(None);
        assert_eq!(
            config.origins.names(),
            vec!["art.example.org", "compute.example.org", "gate.example.org"]
        );
    }

    #[test]
    fn the_staging_directory_is_named_in_what_check_prints() {
        let staging = describe(&config(Some(Tls {
            contact: "mailto:someone@example.org".into(),
            staging: true,
        })));
        assert!(staging.contains("staging"), "{staging}");
        assert!(staging.contains("tls-alpn-01"), "{staging}");
        assert!(describe(&config(None)).contains("loopback"));
    }
}
