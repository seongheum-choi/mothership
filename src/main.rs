//! mothership: runs Claude Code agents for Linear agent sessions and Zulip conversations.

mod agent;
mod app;
mod config;
mod directive;
mod github;
mod host;
mod linear;
mod modes;
mod repos;
mod review;
mod sandbox;
mod session;
mod sessions;
mod signature;
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
    host::reset_run_root(&cfg.home).context("preparing the host run directory")?;
    let stops = [cfg.worktrees_dir.clone(), cfg.home.join("repos")];
    let stops = sandbox::dotenv_stops(&cfg.home, &stops);
    cfg.sandbox.read.extend(stops);
    let tunnel = cfg.tunnel.take();
    let app = app::App::new(cfg)?;
    // Before `/status` reports `draining`: without a handler, SIGUSR1 would end the process.
    tokio::spawn(drain_signals(app.clone(), drain_signal_streams()?));

    if app.cfg.linear.workspace.is_none() {
        tracing::warn!(
            "LINEAR_WORKSPACE is not set: whichever workspace installs the app first is pinned"
        );
    }
    if app.store.read(|s| s.linear.access_token.is_empty()) {
        tracing::warn!(
            "no Linear token yet: open {}/oauth/authorize; webhooks are refused until then",
            app.cfg.base_url
        );
    } else {
        app.linear
            .surface
            .pin_current(&app)
            .await
            .context("pinning the Linear workspace")?;
        // Prompts held for the restart that started this process.
        linear::release(&app);
    }
    if let Some(tunnel) = tunnel {
        tokio::spawn(tunnel.supervise());
    }
    let listener = tokio::net::TcpListener::bind(&app.cfg.bind)
        .await
        .with_context(|| format!("binding {}", app.cfg.bind))?;
    tracing::info!(
        "listening on {} (zulip {}, github {}, repos: {})",
        app.cfg.bind,
        if app.zulip.is_some() { "on" } else { "off" },
        if app.github.is_some() { "on" } else { "off" },
        app.repos()
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

type Signals = (tokio::signal::unix::Signal, tokio::signal::unix::Signal);

fn drain_signal_streams() -> Result<Signals> {
    use tokio::signal::unix::{SignalKind, signal};
    let drain = signal(SignalKind::user_defined1()).context("listening for SIGUSR1")?;
    let resume = signal(SignalKind::user_defined2()).context("listening for SIGUSR2")?;
    Ok((drain, resume))
}

/// SIGUSR1 holds new turns for a restart (`contrib/deploy.sh` sends it while it waits for the
/// running ones); SIGUSR2 ends that and runs what was held.
async fn drain_signals(app: std::sync::Arc<app::App>, (mut drain, mut resume): Signals) {
    loop {
        tokio::select! {
            _ = drain.recv() => {
                app.set_draining(true);
                tracing::info!("draining: new turns wait for the restart");
            }
            _ = resume.recv() => {
                app.set_draining(false);
                tracing::info!("drain ended; running the turns it held");
                linear::release(&app);
            }
        }
    }
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
