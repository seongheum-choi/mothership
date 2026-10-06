//! mothership: runs Claude Code agents for Linear agent sessions and Zulip conversations.

mod agent;
mod app;
mod config;
mod linear;
mod review;
mod sandbox;
mod session;
mod store;
mod tunnel;
mod worktree;
mod zulip;

use anyhow::{Context, Result};
use std::io::IsTerminal;

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
        "listening on {} (zulip {})",
        app.cfg.bind,
        if app.zulip.is_some() { "on" } else { "off" }
    );
    axum::serve(listener, app.router()).await?;
    Ok(())
}
