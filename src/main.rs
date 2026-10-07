//! mothership: runs Claude Code agents for Linear agent sessions and Zulip conversations.

mod agent;
mod app;
mod config;
mod linear;
mod repos;
mod review;
mod sandbox;
mod session;
mod store;
mod tunnel;
mod worktree;
mod zulip;

use anyhow::{Context, Result};
use std::io::IsTerminal;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(std::io::stdout().is_terminal()) // plain text in pm2/systemd logs
        .init();
    let mut cfg = config::Config::load()?;
    let tunnel = cfg.tunnel.take();
    let app = app::App::new(cfg)?;

    if app.store.read(|s| s.linear.access_token.is_empty()) {
        tracing::warn!(
            "no Linear token yet: open {}/oauth/authorize",
            app.cfg.base_url
        );
    }
    if let Some(tunnel) = tunnel {
        tokio::spawn(tunnel.supervise());
    }
    let listener = tokio::net::TcpListener::bind(&app.cfg.bind)
        .await
        .with_context(|| format!("binding {}", app.cfg.bind))?;
    tracing::info!(
        "listening on {} (zulip {}, repos: {})",
        app.cfg.bind,
        if app.zulip.is_some() { "on" } else { "off" },
        app.cfg
            .repos
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    axum::serve(listener, app.router())
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    // The HTTP server stopped accepting requests; stop the agents it left running.
    app.shutdown(Duration::from_secs(10)).await;
    tracing::info!("shutdown complete");
    Ok(())
}

/// Resolves on the first SIGTERM (launchd, pm2, systemd) or SIGINT (Ctrl-C).
async fn shutdown_signal() {
    let term = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(e) => {
                tracing::error!("cannot listen for SIGTERM: {e:#}");
                std::future::pending::<()>().await;
            }
        }
    };

    let int = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("cannot listen for SIGINT: {e:#}");
            std::future::pending::<()>().await;
        }
    };

    tokio::select! {
        () = term => tracing::info!("SIGTERM received; shutting down"),
        () = int => tracing::info!("SIGINT received; shutting down"),
    }
}
