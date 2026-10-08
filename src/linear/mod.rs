//! Linear agent sessions: each issue the agent is delegated or mentioned on gets a worktree,
//! and the agent's work shows up as agent activities in the session.

mod activity;
mod api;
mod cleanup;
mod mcp;
mod oauth;
mod pin;
mod routing;
mod webhook;

pub use mcp::config as mcp_config;

use crate::{
    agent::Launch,
    app::App,
    modes,
    repos::Repo,
    sandbox,
    session::{Outcome, Surface, Update},
    store::SessionRec,
    worktree,
};
use activity::{prefixed, thought, tool_activity};
use anyhow::{Context, Result, bail};
use axum::{
    Router,
    routing::{get, post},
};
use pin::Workspace;
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Default)]
pub struct Linear {
    refresh_lock: tokio::sync::Mutex<()>,
    oauth_state: Mutex<Option<String>>,
    /// The one workspace this instance serves; `None` until a token has been checked.
    workspace: Mutex<Option<Workspace>>,
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/linear-webhook", post(webhook::webhook))
        .route("/webhook", post(webhook::webhook)) // legacy path some Linear apps are configured with
        .route("/oauth/authorize", get(oauth::authorize))
        .route("/callback", get(oauth::callback))
}

impl Linear {
    /// The issue's worktree of a git repository and the branch it is on, created on the
    /// first turn and remembered after that.
    async fn worktree(
        &self,
        app: &App,
        key: &str,
        rec: &SessionRec,
        repo: &Repo,
    ) -> Result<(PathBuf, String)> {
        if let (Some(w), Some(b)) = (&rec.workspace, &rec.branch)
            && w.exists()
        {
            return Ok((w.clone(), b.clone()));
        }
        let branch = match self.branch_name(app, &rec.issue_id).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("[{key}] branchName lookup failed ({e:#}), using identifier");
                rec.identifier.to_lowercase()
            }
        };
        let dir = app.cfg.worktrees_dir.join(&rec.identifier);
        let workspace = worktree::ensure(&repo.path, &dir, &branch, &repo.base_branch).await?;
        let branch = worktree::current_branch(&workspace).await.unwrap_or(branch);
        app.store.update(|s| {
            if let Some(r) = s.sessions.get_mut(key) {
                r.workspace = Some(workspace.clone());
                r.branch = Some(branch.clone());
            }
        });
        Ok((workspace, branch))
    }
}

impl Surface for Linear {
    type Ticket = ();

    async fn launch(&self, app: &Arc<App>, key: &str) -> Result<Launch> {
        let rec = app
            .store
            .read(|s| s.sessions.get(key).cloned())
            .context("unknown agent session")?;
        let repo = rec
            .repo
            .as_deref()
            .and_then(|name| app.cfg.repo(name))
            .context("the session's repository is not configured")?;
        if rec.mode_pending {
            bail!("the session's mode is not settled yet");
        }
        // Read every turn, so an edit applies to the next one; a broken file fails the turn
        // rather than running it without the mode.
        let mode = rec
            .mode
            .as_deref()
            .map(|name| {
                modes::load(&modes::dir(&app.cfg.home), name)
                    .with_context(|| format!("mode `{name}`"))
            })
            .transpose()?;
        let (workspace, place) = if repo.git {
            let (workspace, branch) = self.worktree(app, key, &rec, repo).await?;
            let place = format!("in a git worktree of `{}` on branch `{branch}`", repo.name);
            (workspace, place)
        } else {
            // No worktrees: sessions take turns in the directory itself (the workspace lock).
            let place = format!(
                "directly in `{}` ({}), which is not a git repository",
                repo.name,
                repo.path.display()
            );
            (repo.path.clone(), place)
        };

        let mut system_prompt = format!(
            "You are {}, a Linear agent, working on issue {} \"{}\" ({}) {place}. Everything \
             you write is relayed to the Linear agent session. Images in the issue or its \
             comments (uploads.linear.app links) need auth, so open them with the Linear MCP \
             `extract_images` tool. If you start working on the issue while it is still in \
             triage, backlog or an unstarted status, move it to its team's first started status \
             with the Linear MCP; a delegated issue is moved for you. The worktree, ignored \
             files included, is deleted once the issue is closed, so never keep anything that \
             matters only in paths git ignores.\n\n",
            app.cfg.agent_name, rec.identifier, rec.title, rec.url
        );
        let delivery = repo.instructions(
            app.cfg
                .extra_prompt
                .as_deref()
                .unwrap_or(app.cfg.review.instructions()),
        );
        system_prompt.push_str(&work_instructions(mode.as_ref(), &delivery));

        let plugin_dirs = app.cfg.plugin_dirs();
        let mut readable = vec![workspace.clone(), repo.path.clone()];
        readable.extend(plugin_dirs.iter().cloned());
        readable.extend(app.github.as_ref().map(|g| g.bin_dir.clone()));
        let mut mcp_configs = vec![mcp::config(app, key)?];
        mcp_configs.extend(app.cfg.mcp_configs.iter().cloned());
        mcp_configs.extend(repo.mcp_configs.iter().cloned());
        Ok(Launch {
            settings: sandbox::settings(
                &app.home_dir,
                &readable,
                mode.as_ref().map_or(&[], |m| &m.deny),
                &app.cfg.sandbox,
            ),
            cwd: workspace,
            system_prompt,
            resume: rec.claude_session_id,
            permission_mode: mode
                .as_ref()
                .and_then(|m| m.permission_mode.clone())
                .unwrap_or_else(|| "bypassPermissions".into()),
            model: mode.and_then(|m| m.model),
            mcp_configs,
            plugin_dirs,
            env: app.agent_env("linear"),
        })
    }

    async fn update(&self, app: &Arc<App>, key: &str, update: Update) {
        let (content, ephemeral) = match update {
            Update::Working => (thought("Working on it…"), true),
            Update::Noted => (thought("Got it, adding that to the current work."), true),
            Update::Thought { text, nested } => (thought(&prefixed(&text, nested)), false),
            Update::Progress(line) => (thought(&line), false),
            Update::Tool {
                name,
                input,
                nested,
            } => tool_activity(&prefixed(&name, nested), &input),
        };
        self.activity(app, key, content, ephemeral).await;
    }

    async fn finish(&self, app: &Arc<App>, key: &str, _tickets: Vec<()>, outcome: Outcome) {
        let content = match outcome {
            Outcome::Reply(text) => json!({ "type": "response", "body": text }),
            Outcome::Failed(text) => json!({ "type": "error", "body": text }),
            Outcome::Stopped(note) => json!({
                "type": "response",
                "body": note.as_deref().unwrap_or("Stopped."),
            }),
        };
        self.activity(app, key, content, false).await;
    }
}

/// The mode's instructions, then the repository's delivery instructions. Delivery comes last so
/// a mode cannot bring commits or pull requests back into a non-git repository.
fn work_instructions(mode: Option<&modes::Mode>, delivery: &str) -> String {
    match mode {
        Some(mode) => format!(
            "This session is in the `{}` mode:\n\n{}\n\n{delivery}",
            mode.name, mode.instructions
        ),
        None => delivery.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_instructions_come_after_the_mode() {
        let mut repo = Repo::single("/notes".into(), "main".into());
        repo.git = false;
        let mode = modes::Mode {
            name: "implement".into(),
            instructions: "Open a pull request.".into(),
            ..modes::Mode::default()
        };
        let prompt = work_instructions(Some(&mode), &repo.instructions("Use gh pr create."));
        assert!(prompt.starts_with("This session is in the `implement` mode:\n\nOpen a pull"));
        assert!(prompt.ends_with("say what you changed in your final reply."));
        assert!(!prompt.contains("gh pr create"));
        assert_eq!(work_instructions(None, "Deliver."), "Deliver.");
    }
}
