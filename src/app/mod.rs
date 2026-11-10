//! Shared state and the HTTP surface; what each agent process gets is in `agent.rs`, `/status`
//! in `status.rs`.

mod agent;
mod status;

pub use agent::Place;

use crate::{
    agent::{Runner, limit},
    config::{Config, Live, ModelDefaults},
    gerrit::Gerrit,
    github::{GitHub, UserToken},
    linear::Linear,
    repos::Repo,
    session::Registry,
    store::Store,
    zulip::Zulip,
};
use anyhow::{Context, Result};
use axum::{Router, routing::get};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub struct App {
    /// Settings read at startup.
    pub cfg: Config,
    /// The repository list and model defaults, reloaded when their files change.
    live: Live,
    /// The user's home directory, which agents may only read where their work is.
    pub home_dir: PathBuf,
    pub http: reqwest::Client,
    pub store: Store,
    pub linear: Arc<Registry<Linear>>,
    pub zulip: Option<Arc<Registry<Zulip>>>,
    pub github: Option<GitHub>,
    /// The GitHub App user token agents get as `GH_TOKEN`, when `GITHUB_APP_CLIENT_ID` is set.
    pub github_user: Option<UserToken>,
    pub gerrit: Option<Gerrit>,
    /// Sessions on the same issue share a worktree; one agent at a time per workspace.
    workspace_locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    /// New turns wait for a restart instead of starting (SIGUSR1, until SIGUSR2).
    draining: AtomicBool,
}

impl App {
    pub fn new(cfg: Config) -> Result<Arc<Self>> {
        match agent::check_session_tmp(&cfg.home) {
            Err(e) if std::env::var_os("MOTHERSHIP_ALLOW_LONG_TMPDIR").is_some() => {
                tracing::warn!("{e:#}");
            }
            checked => checked?,
        }
        std::fs::create_dir_all(&cfg.home)
            .with_context(|| format!("creating {}", cfg.home.display()))?;
        let store = Store::open(cfg.home.join("state.json"))?;
        if store.read(|s| s.linear.access_token.is_empty())
            && let Some(seed) = &cfg.linear.seed_tokens
        {
            store.update(|s| s.linear = seed.clone());
        }
        let home_dir = PathBuf::from(std::env::var("HOME").context("HOME is not set")?);
        let live = Live::load(&cfg.home, &home_dir)?;
        let github = cfg
            .github
            .as_ref()
            .map(|_| GitHub::new(&live.repos(), &cfg.home))
            .transpose()
            .context("setting up GitHub feedback")?;
        let gerrit = cfg
            .gerrit
            .as_ref()
            .map(|gerrit| Gerrit::new(gerrit, &live.repos(), &cfg.home))
            .transpose()
            .context("setting up Gerrit")?;
        Ok(Arc::new(Self {
            home_dir,
            live,
            http: reqwest::Client::new(),
            store,
            linear: Arc::new(Registry::new(Linear::default())),
            zulip: cfg.zulip.as_ref().map(|_| Arc::new(Registry::new(Zulip))),
            github,
            github_user: cfg.github_app.clone().map(UserToken::new),
            gerrit,
            workspace_locks: Mutex::default(),
            draining: AtomicBool::new(false),
            cfg,
        }))
    }

    pub fn router(self: &Arc<Self>) -> Router {
        let mut router = Router::new()
            .route("/status", get(status::status))
            .merge(crate::linear::routes());
        if self.zulip.is_some() {
            router = router.merge(crate::zulip::routes());
        }
        if self.github.is_some() {
            router = router.merge(crate::github::routes());
        }
        if self.gerrit.is_some() {
            router = router.merge(crate::gerrit::routes());
        }
        router.with_state(self.clone())
    }

    /// The repositories in effect now; hold on to the list for a consistent view.
    pub fn repos(&self) -> Arc<Vec<Repo>> {
        self.live.repos()
    }

    pub fn repo(&self, name: &str) -> Option<Repo> {
        crate::repos::by_name(&self.repos(), name).cloned()
    }

    /// Picks up changes to `repos.json` and `.env` before an event is routed, so the next turn
    /// sees them. GitHub origins and Gerrit projects follow a changed repository list.
    pub fn refresh(&self) {
        let Some(repos) = self.live.refresh() else {
            return;
        };
        if let Some(github) = &self.github {
            github.set_repos(&repos);
        }
        if let (Some(gerrit), Some(cfg)) = (&self.gerrit, &self.cfg.gerrit)
            && cfg.webhook_token.is_some()
        {
            gerrit.set_repos(cfg, &repos);
        }
    }

    /// The runner of a turn no narrower scope decides: the instance's as the last [`Self::refresh`]
    /// read `.env`, or Codex while Claude Code's usage limit lasts.
    pub fn runner(&self) -> Runner {
        if self.claude_limited_until().is_some() {
            Runner::Codex
        } else {
            self.models().runner
        }
    }

    /// When Claude Code's usage limit resets, while it moves this instance's turns to Codex.
    pub fn claude_limited_until(&self) -> Option<u64> {
        self.claude_limit(self.models().runner)
    }

    /// When Claude Code's usage limit resets, while it moves a turn that `decided` would run
    /// to Codex.
    pub fn claude_limit(&self, decided: Runner) -> Option<u64> {
        let resets_at = self.store.read(|s| s.claude_resets_at)?;
        let applies = limit::applies(decided, resets_at, crate::store::now_secs());
        (applies && self.codex_installed()).then_some(resets_at)
    }

    /// Whether a turn that hit Claude Code's usage limit can move to Codex: `CODEX_BIN` is there
    /// on the `PATH` the agent would run with, not this process's, and `HOST_SANDBOX` to
    /// confine it (see [`crate::host::codex_runner`]).
    pub fn codex_installed(&self) -> bool {
        self.cfg.agent.host_sandbox.is_some()
            && limit::installed(&self.cfg.agent.codex_bin, self.agent_path().as_deref())
    }

    /// The `PATH` agent processes get, when `AGENT_ENV` carries one. Without it they inherit this
    /// process's, which [`limit::installed`] falls back to.
    pub fn agent_path(&self) -> Option<String> {
        self.cfg
            .agent_env
            .iter()
            .rev()
            .find(|(key, _)| key == "PATH")
            .map(|(_, path)| path.clone())
    }

    /// The instance's model defaults as of the last [`Self::refresh`]; every entry point that
    /// cares refreshes once and then reads.
    pub fn models(&self) -> ModelDefaults {
        self.live.models()
    }

    pub async fn lock_workspace(&self, path: &Path) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .workspace_locks
            .lock()
            .expect("workspace lock map poisoned")
            .entry(path.to_path_buf())
            .or_default()
            .clone();
        lock.lock_owned().await
    }

    pub fn draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }

    pub fn set_draining(&self, on: bool) {
        self.draining.store(on, Ordering::Relaxed);
    }

    fn busy(&self) -> bool {
        self.linear.busy() || self.zulip.as_ref().is_some_and(|z| z.busy())
    }

    /// Stops every running session and waits for their agent process trees to be killed,
    /// giving up after `grace`. Returns at once when nothing is running.
    pub async fn shutdown(&self, grace: Duration) {
        let stopped = self.linear.stop_all() + self.zulip.as_ref().map_or(0, |z| z.stop_all());
        if stopped == 0 {
            return;
        }
        tracing::info!(
            "stopping {stopped} running session(s), up to {}s",
            grace.as_secs()
        );
        let drained = async {
            while self.busy() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        if tokio::time::timeout(grace, drained).await.is_err() {
            tracing::warn!("grace period elapsed; exiting with sessions still stopping");
        }
    }
}
