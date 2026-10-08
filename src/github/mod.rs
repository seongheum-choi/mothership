//! GitHub App webhooks: reviews and comments on an agent's pull request continue the Linear
//! session that owns the PR's head branch. Results still go to Linear; the agent answers on
//! GitHub itself through the `gh-reply` wrapper.
//!
//! Comment text reaches an agent that runs without permission prompts, so only the repository
//! owner (and `GITHUB_TRUSTED_LOGINS`) is heard, only when they address the agent's GitHub
//! account, only on same-repository PRs of this instance's git repositories, and each delivery
//! at most once.

mod feedback;
mod guard;
mod reply;
mod route;

use crate::{app::App, repos::Repo};
use anyhow::{Result, bail};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use feedback::Feedback;
use guard::{Budget, Deliveries};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

/// A git repository of this instance and the GitHub repository its `origin` points at.
#[derive(Debug)]
struct Origin {
    /// Name in `repos.json`, as session records store it.
    name: String,
    /// Main clone, where `gh api` runs.
    path: PathBuf,
    /// `owner/name` of its `origin` remote.
    full_name: String,
}

/// The origins of the git repositories, replaced whole when the repository list is reloaded.
#[derive(Debug)]
pub struct Origins(Vec<Origin>);

impl Origins {
    /// Resolves the origins of the git repositories; a repository whose origin is not
    /// `owner/name` is left out.
    fn resolve(repos: &[Repo]) -> Self {
        let mut origins = Vec::new();
        for repo in repos.iter().filter(|r| r.git) {
            match reply::origin_of(&repo.path) {
                Ok(full_name) => origins.push(Origin {
                    name: repo.name.clone(),
                    path: repo.path.clone(),
                    full_name,
                }),
                Err(e) => tracing::warn!("github feedback off for repo {}: {e:#}", repo.name),
            }
        }
        Self(origins)
    }

    fn list(&self) -> String {
        let names: Vec<&str> = self.0.iter().map(|o| o.full_name.as_str()).collect();
        names.join(", ")
    }

    /// Names of the repositories whose origin is `full_name`; usually one.
    fn names(&self, full_name: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|o| o.full_name.eq_ignore_ascii_case(full_name))
            .map(|o| o.name.as_str())
            .collect()
    }

    fn clone_of(&self, full_name: &str) -> Option<&Path> {
        self.0
            .iter()
            .find(|o| o.full_name.eq_ignore_ascii_case(full_name))
            .map(|o| o.path.as_path())
    }
}

/// Runtime state of the GitHub surface.
pub struct GitHub {
    /// Origins of the git repositories; events from any other repository are ignored.
    origins: RwLock<Arc<Origins>>,
    /// Holds `gh-reply`; put first on agents' `PATH`.
    pub bin_dir: PathBuf,
    deliveries: Mutex<Deliveries>,
    budget: Mutex<Budget>,
}

impl GitHub {
    /// Resolves the origins of the git repositories and installs `<home>/gh-reply/gh-reply`.
    /// No origin at all is an error.
    pub fn new(repos: &[Repo], home: &Path) -> Result<Self> {
        let origins = Origins::resolve(repos);
        if origins.0.is_empty() {
            bail!("no git repository has a GitHub origin");
        }
        let bin_dir = reply::install(home)?;
        tracing::info!("github feedback accepted for {}", origins.list());
        Ok(Self {
            origins: RwLock::new(Arc::new(origins)),
            bin_dir,
            deliveries: Mutex::default(),
            budget: Mutex::default(),
        })
    }

    fn origins(&self) -> Arc<Origins> {
        self.origins.read().expect("origins poisoned").clone()
    }

    /// Resolves the origins again for a reloaded repository list.
    pub fn set_repos(&self, repos: &[Repo]) {
        let origins = Origins::resolve(repos);
        if origins.0.is_empty() {
            tracing::warn!("github feedback: no repository has a GitHub origin any more");
        } else {
            tracing::info!("github feedback accepted for {}", origins.list());
        }
        *self.origins.write().expect("origins poisoned") = Arc::new(origins);
    }
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/github-webhook", post(webhook))
}

async fn webhook(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let (Some(cfg), Some(github)) = (&app.cfg.github, &app.github) else {
        return StatusCode::NOT_FOUND;
    };
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    };
    if !verify(&cfg.webhook_secret, &body, header("x-hub-signature-256")) {
        return StatusCode::UNAUTHORIZED;
    }
    let delivery = header("x-github-delivery");
    if !github
        .deliveries
        .lock()
        .expect("delivery set poisoned")
        .first_time(delivery)
    {
        tracing::info!("github delivery {delivery} seen before, ignored");
        return StatusCode::OK;
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    let Some(feedback) = Feedback::parse(header("x-github-event"), &payload) else {
        return StatusCode::OK;
    };
    app.refresh();
    if let Err(reason) = feedback.screen(!github.origins().names(&feedback.repo).is_empty(), cfg) {
        // Who and what kind only: the text is untrusted and may be long.
        tracing::info!(
            "github {} by @{} ({}): {reason}, ignored",
            feedback.kind.label(),
            feedback.author,
            feedback.association
        );
        return StatusCode::OK;
    }
    // GitHub gives up after 10 seconds; the lookup and the turn happen in the background.
    tokio::spawn(route::handle(app.clone(), feedback));
    StatusCode::OK
}

/// `X-Hub-Signature-256` is `sha256=` + hex(HMAC-SHA256(secret, raw body)).
fn verify(secret: &str, body: &[u8], signature: &str) -> bool {
    signature
        .strip_prefix("sha256=")
        .is_some_and(|hex| crate::signature::hmac_sha256_hex_matches(secret, body, hex))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature() {
        let body = br#"{"zen":"hi"}"#;
        // python3 -c 'import hmac,hashlib;print(hmac.new(b"s",b"{\"zen\":\"hi\"}",hashlib.sha256).hexdigest())'
        let hex = "5b64481908428a9d02cbf79c42a7777672f9ec84641e7e227a2aab5637b35bb2";
        let sig = format!("sha256={hex}");
        assert!(verify("s", body, &sig));
        assert!(!verify("t", body, &sig), "wrong secret");
        assert!(!verify("s", br#"{"zen":"ho"}"#, &sig), "changed body");
        assert!(!verify("s", body, hex), "no sha256= prefix");
        assert!(!verify("s", body, "sha256=zz"), "not hex");
        assert!(!verify("s", body, ""), "missing header");
    }

    /// Repositories `app` and `site` (both on `o/r`, say a second clone on another base branch)
    /// and `lib` on `o/lib`.
    pub(super) fn origins() -> Origins {
        let origin = |name: &str, full_name: &str| Origin {
            name: name.into(),
            path: PathBuf::from(format!("/src/{name}")),
            full_name: full_name.into(),
        };
        Origins(vec![
            origin("app", "o/r"),
            origin("site", "o/r"),
            origin("lib", "o/lib"),
        ])
    }

    #[test]
    fn origins_map_to_repository_names_and_clones() {
        let gh = origins();
        assert_eq!(gh.names("O/R"), ["app", "site"]);
        assert_eq!(gh.names("o/lib"), ["lib"]);
        let none: [&str; 0] = [];
        assert_eq!(gh.names("o/other"), none);
        assert_eq!(gh.clone_of("o/lib"), Some(Path::new("/src/lib")));
        assert_eq!(gh.clone_of("o/other"), None);
    }
}
