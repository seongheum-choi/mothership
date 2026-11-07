//! Gerrit review: agents push changes and answer review comments on Gerrit with the account in
//! `GERRIT_USERNAME`, through git and the `gerrit-reply` wrapper. Reviews come back through
//! `POST /gerrit-webhook` as `comment-added` events and continue the Linear session of the
//! issue whose key is the change's topic.
//!
//! Review text reaches an agent that runs without permission prompts, so only requests with the
//! webhook token are read, only reviews by `GERRIT_TRUSTED_EMAILS` are heard, only when they
//! mention the agent, and only on changes of this instance's git repositories.

mod api;
mod event;
mod project;
mod prompt;
mod reply;
mod route;

use crate::{app::App, config::GerritConfig, github::Budget, repos::Repo};
use anyhow::Result;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use event::Event;
use project::Projects;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

/// Runtime state of the Gerrit backend.
pub struct Gerrit {
    /// Holds `gerrit-reply`; put first on agents' `PATH`.
    pub bin_dir: PathBuf,
    /// The Gerrit projects of the git repositories; reviews on any other are ignored.
    projects: RwLock<Arc<Projects>>,
    budget: Mutex<Budget>,
}

impl Gerrit {
    /// Installs `<home>/gerrit-reply/gerrit-reply` and finds the repositories' projects.
    pub fn new(cfg: &GerritConfig, repos: &[Repo], home: &Path) -> Result<Self> {
        let gerrit = Self {
            bin_dir: reply::install(home)?,
            projects: RwLock::default(),
            budget: Mutex::default(),
        };
        if cfg.webhook_token.is_some() {
            gerrit.set_repos(cfg, repos);
        }
        Ok(gerrit)
    }

    fn projects(&self) -> Arc<Projects> {
        self.projects.read().expect("projects poisoned").clone()
    }

    /// Finds the projects again for a reloaded repository list.
    pub fn set_repos(&self, cfg: &GerritConfig, repos: &[Repo]) {
        let projects = Projects::resolve(repos, &cfg.url);
        if projects.is_empty() {
            tracing::warn!(
                "gerrit feedback: no repository has an origin on {}",
                cfg.url
            );
        } else {
            tracing::info!("gerrit feedback accepted for {}", projects.list());
        }
        *self.projects.write().expect("projects poisoned") = Arc::new(projects);
    }
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/gerrit-webhook", post(webhook))
}

async fn webhook(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let (Some(cfg), Some(gerrit)) = (&app.cfg.gerrit, &app.gerrit) else {
        return StatusCode::NOT_FOUND;
    };
    let Some(token) = &cfg.webhook_token else {
        return StatusCode::NOT_FOUND;
    };
    let given = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !crate::signature::constant_time_eq(given.as_bytes(), token.as_bytes()) {
        return StatusCode::UNAUTHORIZED;
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    let Some(event) = Event::parse(&payload) else {
        return StatusCode::OK;
    };
    app.refresh();
    let watched = !gerrit.projects().names(&event.project).is_empty();
    if let Err(reason) = event.screen(watched, cfg) {
        // Who and where only: the text is untrusted and may be long.
        tracing::debug!(
            "gerrit review by {} on {}: {reason}, ignored",
            event.author.label(),
            event.url
        );
        return StatusCode::OK;
    }
    // The REST calls and the turn happen in the background; the hook need not wait for them.
    tokio::spawn(route::handle(app.clone(), event));
    StatusCode::OK
}

/// Adds what an agent needs to push to and answer on Gerrit to its environment `env`: the URL,
/// the account's HTTP credentials, and a git credential helper that hands them to git for URLs
/// under `GERRIT_URL` only, in place of any helper the global or system config has there. `inherited` is the `GIT_CONFIG_COUNT` the agent would otherwise get.
pub fn agent_env(cfg: &GerritConfig, env: &mut Vec<(String, String)>, inherited: Option<String>) {
    // Configuration through the environment adds to entries `AGENT_ENV` or mothership's own
    // environment already numbered, instead of replacing them.
    let count = env
        .iter()
        .rev()
        .find(|(k, _)| k == "GIT_CONFIG_COUNT")
        .map(|(_, v)| v.clone())
        .or(inherited)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let key = format!("credential.{}.helper", cfg.url);
    env.extend([
        ("GERRIT_URL".into(), cfg.url.clone()),
        ("GERRIT_USERNAME".into(), cfg.username.clone()),
        ("GERRIT_HTTP_PASSWORD".into(), cfg.http_password.clone()),
        // An empty helper empties the list first: a global `store` or keychain helper would
        // otherwise be asked before ours and be handed the password to save after a push.
        (format!("GIT_CONFIG_KEY_{count}"), key.clone()),
        (format!("GIT_CONFIG_VALUE_{count}"), String::new()),
        (format!("GIT_CONFIG_KEY_{}", count + 1), key),
        (
            format!("GIT_CONFIG_VALUE_{}", count + 1),
            CREDENTIAL_HELPER.into(),
        ),
        ("GIT_CONFIG_COUNT".into(), (count + 2).to_string()),
    ]);
}

/// Answers git's `get` with the credentials from the environment, so they are never written to
/// a file in the agent's reach; `store` and `erase` do nothing.
const CREDENTIAL_HELPER: &str = "!f() { if [ \"$1\" = get ]; then printf 'username=%s\\npassword=%s\\n' \
     \"$GERRIT_USERNAME\" \"$GERRIT_HTTP_PASSWORD\"; fi; }; f";

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn cfg() -> GerritConfig {
        GerritConfig {
            url: "https://gerrit.example.com".into(),
            username: "acme-bot".into(),
            http_password: "secret".into(),
            webhook_token: Some("token".into()),
            trusted_emails: vec!["Alice@Example.com".into()],
            mention: "acme-bot".into(),
        }
    }

    /// A `comment-added` event by Alice on change 12, patch set 2, topic `SH-8`.
    pub(super) fn comment_added() -> Value {
        serde_json::json!({
            "type": "comment-added",
            "change": {
                "project": "team/app", "branch": "main", "topic": "SH-8", "number": 12,
                "url": "https://gerrit.example.com/c/team/app/+/12", "status": "NEW",
            },
            "patchSet": {"number": 2, "revision": "abc"},
            "author": {"name": "Alice", "email": "alice@example.com", "username": "alice"},
            "comment": "Patch Set 2: Code-Review-1\n\n(1 comment)",
            "eventCreatedOn": 1_700_000_000,
        })
    }

    pub(super) fn event() -> Event {
        Event::parse(&comment_added()).expect("comment-added parses")
    }

    fn get<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn credential_helper_takes_the_next_free_git_config_slot() {
        let mut env = Vec::new();
        agent_env(&cfg(), &mut env, None);
        assert_eq!(get(&env, "GIT_CONFIG_COUNT"), Some("2"));
        for key in ["GIT_CONFIG_KEY_0", "GIT_CONFIG_KEY_1"] {
            assert_eq!(
                get(&env, key),
                Some("credential.https://gerrit.example.com.helper")
            );
        }
        assert_eq!(
            get(&env, "GIT_CONFIG_VALUE_0"),
            Some(""),
            "empties the list"
        );
        assert_eq!(get(&env, "GIT_CONFIG_VALUE_1"), Some(CREDENTIAL_HELPER));
        assert_eq!(get(&env, "GERRIT_HTTP_PASSWORD"), Some("secret"));

        let mut env = vec![("GIT_CONFIG_COUNT".to_string(), "2".to_string())];
        agent_env(&cfg(), &mut env, Some("5".into()));
        assert_eq!(get(&env, "GIT_CONFIG_COUNT"), Some("4"), "AGENT_ENV wins");
        assert!(get(&env, "GIT_CONFIG_KEY_3").is_some());

        let mut env = Vec::new();
        agent_env(&cfg(), &mut env, Some("1".into()));
        assert_eq!(get(&env, "GIT_CONFIG_COUNT"), Some("3"), "inherited");
        assert!(get(&env, "GIT_CONFIG_KEY_2").is_some());
    }

    #[test]
    fn git_gets_the_credentials_for_gerrit_urls_only() {
        // A global helper that answers every URL and logs each operation it is asked for, as
        // `store` or a keychain would be asked to save the password after a push.
        let dir = std::env::temp_dir().join(format!("mothership-gerrit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let log = dir.join("global-helper.log");
        let global = dir.join("gitconfig");
        let helper = format!(
            "!f() {{ echo \"$1\" >> '{}'; echo username=global; echo password=global-secret; }}; f",
            log.display()
        );
        std::fs::write(&global, format!("[credential]\n\thelper = \"{helper}\"\n"))
            .expect("writing the global config");
        let mut env = Vec::new();
        agent_env(&cfg(), &mut env, None);
        let git = |op: &str, input: &str| {
            let mut child = std::process::Command::new("git")
                .args(["credential", op])
                .envs(env.iter().cloned())
                .env("GIT_CONFIG_GLOBAL", &global)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_ASKPASS", "false")
                .env_remove("SSH_ASKPASS")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("git runs");
            let mut stdin = child.stdin.take().expect("piped stdin");
            std::io::Write::write_all(&mut stdin, input.as_bytes()).expect("git reads its input");
            drop(stdin);
            let out = child.wait_with_output().expect("git finishes");
            String::from_utf8(out.stdout).expect("utf-8")
        };
        let logged = || std::fs::read_to_string(&log).unwrap_or_default();

        let gerrit = "url=https://gerrit.example.com/a/team/app\n";
        let found = git("fill", &format!("{gerrit}\n"));
        assert!(
            found.contains("username=acme-bot\npassword=secret\n"),
            "{found}"
        );
        git(
            "approve",
            &format!("{gerrit}username=acme-bot\npassword=secret\n\n"),
        );
        assert_eq!(logged(), "", "the global helper never hears of Gerrit");

        let found = git("fill", "url=https://github.com/o/r\n\n");
        assert!(found.contains("password=global-secret"), "{found}");
        assert!(!found.contains("password=secret"));
        assert_eq!(logged(), "get\n", "other hosts keep the global helper");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
