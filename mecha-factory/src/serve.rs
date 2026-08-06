//! Binding sockets, and stopping cleanly.
//!
//! Two shapes, one router. In production a single listener carries all three
//! origins, told apart by `Host` — which is how three names on one address
//! works and what a certificate for three names is for. In dev, three loopback
//! ports carry the same three roles, because there is no DNS on a laptop and
//! `127.0.0.1:8401` is a perfectly good `Host` value. **The router, the origin
//! resolution and the headers are identical in both.** A dev mode that took a
//! different path would be verifying a program nobody deploys.
//!
//! `ConnectInfo` is wired because the rate limiter needs the peer address. With
//! no proxy in front, the peer *is* the client — which is one of the quieter
//! benefits of §13.2's "no CDN to start": there is no `X-Forwarded-For` to
//! decide whether to believe, and a header a stranger can set is not something
//! this server ever reads.

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;

use crate::config::Config;
use crate::db::Db;
use crate::http::{router, App};

/// Serve until Ctrl-C or SIGTERM.
pub async fn run(config: Config, dev: bool) -> Result<()> {
    let db = Db::open(&config.db_path())?;
    let addresses = if dev {
        // One listener per role, so a local run exercises three origins.
        let base = config.listen.https;
        vec![
            base,
            SocketAddr::new(base.ip(), base.port() + 1),
            SocketAddr::new(base.ip(), base.port() + 2),
        ]
    } else {
        vec![config.listen.https]
    };

    let tls = config.tls.is_some();
    let app = Arc::new(App::new(config, db)?);
    for role in [
        crate::config::Role::Gate,
        crate::config::Role::Artifacts,
        crate::config::Role::Compute,
    ] {
        tracing::info!(role = role.as_str(), url = %app.config.base_url(role), "serving");
    }

    if tls {
        // One listener, three names, told apart by `Host` — which is what a
        // certificate for three names is for.
        let config = app.config.clone();
        return crate::tls::serve(app, &config).await;
    }

    let mut listeners = Vec::new();
    for address in addresses {
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .with_context(|| format!("binding {address}"))?;
        tracing::info!(%address, "listening");
        listeners.push(listener);
    }
    run_on(app, listeners).await
}

/// Serve on sockets somebody else bound.
///
/// What the tests drive: they bind port 0 first so the kernel picks free ports,
/// then build the origin table out of the addresses they actually got. Without
/// this split a test would have to guess at ports, and two tests running at
/// once would collide — which is how a suite acquires a flake that looks like a
/// server bug.
pub async fn run_on(app: Arc<App>, listeners: Vec<tokio::net::TcpListener>) -> Result<()> {
    let mut servers = Vec::new();
    for listener in listeners {
        let service = router(app.clone()).into_make_service_with_connect_info::<SocketAddr>();
        servers.push(tokio::spawn(async move {
            axum::serve(listener, service)
                .with_graceful_shutdown(shutdown())
                .await
        }));
    }
    for server in servers {
        server.await.context("a listener panicked")??;
    }
    Ok(())
}

/// Ctrl-C or SIGTERM. The second one is what systemd sends, and a server that
/// only handled the first would be killed rather than stopped on every deploy.
async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("stopping");
}
