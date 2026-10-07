//! Linear agent sessions: each issue the agent is delegated or mentioned on gets a worktree,
//! and the agent's work shows up as agent activities in the session.

mod api;

use crate::{
    agent::Launch,
    app::App,
    sandbox,
    session::{Outcome, Surface, Update},
    store::{SessionRec, random_hex},
    worktree,
};
use anyhow::{Context, Result};
use axum::{
    Router,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::Redirect,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, sync::Arc, sync::Mutex};

#[derive(Default)]
pub struct Linear {
    refresh_lock: tokio::sync::Mutex<()>,
    oauth_state: Mutex<Option<String>>,
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/linear-webhook", post(webhook))
        .route("/webhook", post(webhook)) // legacy path some Linear apps are configured with
        .route("/oauth/authorize", get(authorize))
        .route("/callback", get(callback))
}

async fn webhook(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let signature = headers
        .get("linear-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    if !api::verify(&app.cfg.linear.webhook_secret, &body, signature, now_ms) {
        return StatusCode::UNAUTHORIZED;
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    // Linear wants a 200 within 5 seconds; the work happens in the background.
    tokio::spawn(handle(app, payload));
    StatusCode::OK
}

async fn handle(app: Arc<App>, p: Value) {
    if p["type"] != "AgentSessionEvent" {
        return;
    }
    let (Some(sid), issue) = (
        p["agentSession"]["id"].as_str(),
        &p["agentSession"]["issue"],
    ) else {
        return;
    };
    if issue.is_null() {
        return;
    }
    let prompt = match p["action"].as_str() {
        Some("created") => initial_prompt(&p),
        Some("prompted") if p["agentActivity"]["signal"] == "stop" => {
            if !app.linear.stop(sid) {
                app.linear
                    .surface
                    .finish(&app, sid, Vec::new(), Outcome::Stopped)
                    .await;
            }
            return;
        }
        Some("prompted") => p["agentActivity"]["content"]["body"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        _ => return,
    };
    tracing::info!("[{sid}] linear {} on {}", p["action"], issue["identifier"]);
    let field = |k: &str| issue[k].as_str().unwrap_or_default().to_string();
    app.store.update(|s| {
        let rec = s.sessions.entry(sid.to_string()).or_default();
        if rec.issue_id.is_empty() {
            *rec = SessionRec {
                issue_id: field("id"),
                identifier: field("identifier"),
                title: field("title"),
                url: field("url"),
                ..std::mem::take(rec)
            };
        }
    });
    app.linear.submit(&app, sid, prompt, ());
}

/// Linear's own `promptContext` (issue, comments, guidance) when present, else a minimal one.
fn initial_prompt(p: &Value) -> String {
    if let Some(ctx) = p["promptContext"].as_str() {
        return ctx.to_string();
    }
    let issue = &p["agentSession"]["issue"];
    let mut prompt = format!(
        "{}: {}\n\n{}",
        issue["identifier"].as_str().unwrap_or_default(),
        issue["title"].as_str().unwrap_or_default(),
        issue["description"].as_str().unwrap_or_default()
    );
    // A session started by an @mention carries the mention as its comment.
    let comment = p["agentSession"]["comment"]["body"]
        .as_str()
        .unwrap_or_default();
    if !comment.is_empty() && !comment.contains("This thread is for an agent session") {
        prompt.push_str("\n\nRequest:\n");
        prompt.push_str(comment);
    }
    prompt
}

impl Surface for Linear {
    type Ticket = ();

    async fn launch(&self, app: &Arc<App>, key: &str) -> Result<Launch> {
        let rec = app
            .store
            .read(|s| s.sessions.get(key).cloned())
            .context("unknown agent session")?;
        let (workspace, branch) = match (&rec.workspace, &rec.branch) {
            (Some(w), Some(b)) if w.exists() => (w.clone(), b.clone()),
            _ => {
                let branch = match self.branch_name(app, &rec.issue_id).await {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            "[{key}] branchName lookup failed ({e:#}), using identifier"
                        );
                        rec.identifier.to_lowercase()
                    }
                };
                let dir = app.cfg.worktrees_dir.join(&rec.identifier);
                let workspace =
                    worktree::ensure(&app.cfg.repo, &dir, &branch, &app.cfg.base_branch).await?;
                let branch = worktree::current_branch(&workspace).await.unwrap_or(branch);
                app.store.update(|s| {
                    if let Some(r) = s.sessions.get_mut(key) {
                        r.workspace = Some(workspace.clone());
                        r.branch = Some(branch.clone());
                    }
                });
                (workspace, branch)
            }
        };

        let mut system_prompt = format!(
            "You are {}, a Linear agent, working on issue {} \"{}\" ({}) in a git worktree on \
             branch `{branch}`. Everything you write is relayed to the Linear agent session.\n\n",
            app.cfg.agent_name, rec.identifier, rec.title, rec.url
        );
        system_prompt.push_str(
            app.cfg
                .extra_prompt
                .as_deref()
                .unwrap_or(app.cfg.review.instructions()),
        );

        let plugin_dirs = app.cfg.plugin_dirs();
        let mut readable = vec![workspace.clone(), app.cfg.repo.clone()];
        readable.extend(plugin_dirs.iter().cloned());
        let mut mcp_configs = vec![app.linear_mcp_config(key)?];
        mcp_configs.extend(app.cfg.mcp_configs.iter().cloned());
        Ok(Launch {
            settings: sandbox::settings(&app.home_dir, &readable, &[]),
            cwd: workspace,
            system_prompt,
            resume: rec.claude_session_id,
            permission_mode: "bypassPermissions".into(),
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
            Outcome::Stopped => json!({ "type": "response", "body": "Stopped." }),
        };
        self.activity(app, key, content, false).await;
    }
}

fn thought(body: &str) -> Value {
    json!({ "type": "thought", "body": body })
}

/// Subagent output is marked so it reads as part of the step that started it.
fn prefixed(text: &str, nested: bool) -> String {
    if nested {
        format!("↪ {text}")
    } else {
        text.to_string()
    }
}

/// `TodoWrite` becomes a persistent checklist; every other tool call is an ephemeral action.
fn tool_activity(name: &str, input: &Value) -> (Value, bool) {
    if name.ends_with("TodoWrite") {
        let body = input["todos"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| {
                let content = t["content"].as_str().unwrap_or_default();
                match t["status"].as_str() {
                    Some("completed") => format!("- [x] {content}"),
                    Some("in_progress") => format!("- [ ] {content} (in progress)"),
                    _ => format!("- [ ] {content}"),
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        return (thought(&body), false);
    }
    let parameter = [
        "command",
        "file_path",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
        "skill",
    ]
    .iter()
    .find_map(|k| input[k].as_str().map(String::from))
    .unwrap_or_else(|| input.to_string());
    let parameter: String = parameter.chars().take(300).collect();
    (
        json!({ "type": "action", "action": name, "parameter": parameter }),
        true,
    )
}

/// Only installs into an empty token store: the endpoint is public, so once installed nobody
/// can swap in their own workspace. To install again, clear `linear` in state.json and restart.
async fn authorize(State(app): State<Arc<App>>) -> Result<Redirect, (StatusCode, &'static str)> {
    if app.store.read(|s| !s.linear.access_token.is_empty()) {
        return Err((StatusCode::FORBIDDEN, "already installed"));
    }
    let state = random_hex(16);
    *app.linear
        .surface
        .oauth_state
        .lock()
        .expect("oauth lock poisoned") = Some(state.clone());
    let redirect_uri = format!("{}/callback", app.cfg.base_url);
    let url = reqwest::Url::parse_with_params(
        "https://linear.app/oauth/authorize",
        [
            ("client_id", app.cfg.linear.client_id.as_str()),
            ("redirect_uri", &redirect_uri),
            ("response_type", "code"),
            ("scope", "read,write,app:assignable,app:mentionable"),
            ("actor", "app"),
            ("state", &state),
        ],
    )
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "bad authorize URL"))?;
    Ok(Redirect::to(url.as_str()))
}

async fn callback(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> (StatusCode, String) {
    let linear = &app.linear.surface;
    let expected = linear
        .oauth_state
        .lock()
        .expect("oauth lock poisoned")
        .take();
    if expected.is_none() || q.get("state") != expected.as_ref() {
        return (
            StatusCode::BAD_REQUEST,
            "state mismatch; start again at /oauth/authorize".into(),
        );
    }
    let Some(code) = q.get("code") else {
        return (StatusCode::BAD_REQUEST, "missing code".into());
    };
    match linear.exchange_code(&app, code).await {
        Ok(tokens) => {
            app.store.update(|s| s.linear = tokens);
            (
                StatusCode::OK,
                "Linear authorized. You can close this tab.".into(),
            )
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("token exchange failed: {e:#}"),
        ),
    }
}

impl App {
    /// Linear's hosted MCP with the app token, rewritten per turn so it carries the current token.
    // ponytail: a turn longer than the 24h token lifetime loses Linear MCP mid-turn; proxy MCP through mothership if that ever happens.
    pub fn linear_mcp_config(&self, key: &str) -> Result<PathBuf> {
        let token = self.store.read(|s| s.linear.access_token.clone());
        let config = json!({ "mcpServers": { "linear": {
            "type": "http",
            "url": "https://mcp.linear.app/mcp",
            "headers": { "Authorization": format!("Bearer {token}") },
        }}});
        let dir = self.cfg.home.join("mcp");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", crate::store::file_name(key)));
        crate::store::write_private(&path, &serde_json::to_vec(&config)?)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mention_prompt_includes_request() {
        let p = json!({"agentSession":{"issue":{"identifier":"A-1","title":"T","description":"D"},"comment":{"body":"@bot fix it"}}});
        assert_eq!(initial_prompt(&p), "A-1: T\n\nD\n\nRequest:\n@bot fix it");
        let p = json!({"promptContext":"<issue/>","agentSession":{}});
        assert_eq!(initial_prompt(&p), "<issue/>");
    }

    #[test]
    fn todo_write_becomes_a_checklist() {
        let input = json!({"todos":[{"content":"a","status":"completed"},{"content":"b","status":"in_progress"}]});
        assert_eq!(
            tool_activity("TodoWrite", &input),
            (thought("- [x] a\n- [ ] b (in progress)"), false)
        );
        assert_eq!(
            tool_activity("Bash", &json!({"command":"ls"})),
            (
                json!({"type":"action","action":"Bash","parameter":"ls"}),
                true
            )
        );
    }
}
