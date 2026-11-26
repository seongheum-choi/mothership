//! mothership: runs Claude Code agents for Linear agent sessions and Zulip conversations, and
//! its command line asks a running instance about them.

mod agent;
mod app;
mod cli;
mod config;
mod control;
mod directive;
mod gerrit;
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
use std::process::ExitCode;
use std::time::Duration;

/// Arguments are read before anything else: a `session` command or `--help` must not touch
/// the home directory or the port of the instance that is already running.
fn main() -> Result<ExitCode> {
    match cli::Command::parse(std::env::args().skip(1)) {
        Some(cli::Command::Serve) => serve().map(|()| ExitCode::SUCCESS),
        Some(cli::Command::Session(session)) => cli::session(session),
        Some(cli::Command::Help) => Ok(cli::usage(true)),
        None => Ok(cli::usage(false)),
    }
}

#[tokio::main]
async fn serve() -> Result<()> {
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

    if app.cfg.agent.host_sandbox.is_none() {
        tracing::warn!(
            "HOST_SANDBOX is not a file: host runs and Codex turns are off, so sessions get no \
             MOTHERSHIP_RUNS and no repository commands, and Claude's usage limit fails turns"
        );
    }
    let allowlist = sandbox::allowed_domains(&app.cfg.home);
    if allowlist.exists() {
        tracing::warn!(
            "allowed-domains limits only Claude Code's Bash: Codex turns and host runs still \
             reach any host"
        );
    } else {
        tracing::warn!(
            "{} is missing: Claude Code's Bash, like Codex turns and host runs, may reach any \
             host, so an agent turned by a prompt injection can send out what it reads; \
             contrib/allowed-domains is a list to start from",
            allowlist.display()
        );
    }
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
    github::start_login(&app);
    if let Some(tunnel) = tunnel {
        tokio::spawn(tunnel.supervise());
    }
    let listener = tokio::net::TcpListener::bind(&app.cfg.bind)
        .await
        .with_context(|| format!("binding {}", app.cfg.bind))?;
    // After the port: a second instance on this home stops there, before it takes the socket.
    let control = control::listen(&app)
        .inspect_err(|e| tracing::warn!("`mothership session` is off: {e:#}"))
        .is_ok();
    tracing::info!(
        "listening on {} (zulip {}, github {}, gerrit {}, repos: {})",
        app.cfg.bind,
        if app.zulip.is_some() { "on" } else { "off" },
        if app.github.is_some() { "on" } else { "off" },
        if app.gerrit.is_some() { "on" } else { "off" },
        repos::names(&app.repos())
    );
    axum::serve(
        listener,
        app.router()
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    if control {
        control::remove(&app.cfg.home);
    }
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

/// SIGUSR1 holds new turns for a restart (`contrib/deploy.sh` sends it once the instance is
/// idle); SIGUSR2 ends that and runs what was held.
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
