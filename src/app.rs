//! Shared state and the HTTP surface.

use crate::{config::Config, linear::Linear, session::Registry, store::Store, zulip::Zulip};
use anyhow::{Context, Result};
use axum::{Json, Router, extract::State, routing::get};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub struct App {
    pub cfg: Config,
    /// The user's home directory, which agents may only read where their work is.
    pub home_dir: PathBuf,
    pub http: reqwest::Client,
    pub store: Store,
    pub linear: Arc<Registry<Linear>>,
    pub zulip: Option<Arc<Registry<Zulip>>>,
    /// Sessions on the same issue share a worktree; one agent at a time per workspace.
    workspace_locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

impl App {
    pub fn new(cfg: Config) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.home)
            .with_context(|| format!("creating {}", cfg.home.display()))?;
        let store = Store::open(cfg.home.join("state.json"))?;
        if store.read(|s| s.linear.access_token.is_empty())
            && let Some(seed) = &cfg.linear.seed_tokens
        {
            store.update(|s| s.linear = seed.clone());
        }
        Ok(Arc::new(Self {
            home_dir: PathBuf::from(std::env::var("HOME").context("HOME is not set")?),
            http: reqwest::Client::new(),
            store,
            linear: Arc::new(Registry::new(Linear::default())),
            zulip: cfg.zulip.as_ref().map(|_| Arc::new(Registry::new(Zulip))),
            workspace_locks: Mutex::default(),
            cfg,
        }))
    }

    pub fn router(self: &Arc<Self>) -> Router {
        let mut router = Router::new()
            .route("/status", get(status))
            .merge(crate::linear::routes());
        if self.zulip.is_some() {
            router = router.merge(crate::zulip::routes());
        }
        router.with_state(self.clone())
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

    /// Environment every agent process gets: the `MOTHERSHIP_*` markers so hooks and skills know
    /// where they run, plus the `AGENT_ENV` keys resolved from the environment or `<home>/.env`.
    pub fn agent_env(&self, surface: &str) -> Vec<(String, String)> {
        let mut env = vec![
            ("MOTHERSHIP_AGENT".into(), self.cfg.agent_name.clone()),
            ("MOTHERSHIP_SURFACE".into(), surface.into()),
        ];
        env.extend(self.cfg.agent_env.iter().cloned());
        env
    }

    fn busy(&self) -> bool {
        self.linear.busy() || self.zulip.as_ref().is_some_and(|z| z.busy())
    }
}

async fn status(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!({ "status": if app.busy() { "busy" } else { "idle" } }))
}
