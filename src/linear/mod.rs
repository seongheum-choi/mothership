//! Linear agent sessions: each issue the agent is delegated or mentioned on gets a worktree,
//! and the agent's work shows up as agent activities in the session.

mod api;

use crate::{
    agent::Launch,
    app::App,
    repos::{self, IssueFacts, Ref, Repo},
    sandbox,
    session::{Outcome, Surface, Update},
    store::{SessionRec, random_hex},
    worktree,
};
use anyhow::{Context, Result, bail};
use api::Workspace;
use axum::{
    Router,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::Redirect,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Component, Path, PathBuf},
    sync::Arc,
    sync::Mutex,
};

#[derive(Default)]
pub struct Linear {
    refresh_lock: tokio::sync::Mutex<()>,
    oauth_state: Mutex<Option<String>>,
    /// The one workspace this instance serves; `None` until a token has been checked.
    workspace: Mutex<Option<Workspace>>,
}

impl Linear {
    /// Looks up the stored token's workspace and pins it. Errors when it is not the
    /// configured `LINEAR_WORKSPACE`.
    pub async fn pin_current(&self, app: &App) -> Result<()> {
        let ws = self.current_workspace(app).await?;
        self.pin(app, ws)
    }

    /// Pins `ws` unless `LINEAR_WORKSPACE` names another one, or a different workspace is
    /// already pinned: an instance never switches workspaces while running.
    fn pin(&self, app: &App, ws: Workspace) -> Result<()> {
        let mut pinned = self.workspace.lock().expect("workspace lock poisoned");
        check_pin(app.cfg.linear.workspace.as_deref(), pinned.as_ref(), &ws)?;
        if pinned.is_none() {
            tracing::info!("Linear workspace: {ws}");
        }
        *pinned = Some(ws);
        Ok(())
    }

    fn pinned_id(&self) -> Option<String> {
        self.workspace
            .lock()
            .expect("workspace lock poisoned")
            .as_ref()
            .map(|w| w.id.clone())
    }
}

/// Whether `ws` may be pinned, given the `LINEAR_WORKSPACE` setting and the workspace
/// already pinned, if any.
fn check_pin(want: Option<&str>, pinned: Option<&Workspace>, ws: &Workspace) -> Result<()> {
    if let Some(want) = want
        && !ws.is(want)
    {
        bail!("Linear token belongs to workspace {ws}, not LINEAR_WORKSPACE={want}");
    }
    if let Some(p) = pinned
        && p.id != ws.id
    {
        bail!("Linear workspace {p} is pinned; refusing {ws}");
    }
    Ok(())
}

/// Whether a webhook from `org` may be handled: only the pinned workspace's are. Before
/// anything is pinned, every webhook is refused because nothing says whose it is.
fn admits(pinned: Option<&str>, org: Option<&str>) -> bool {
    pinned.is_some() && org == pinned
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
    let event = payload["type"].as_str().unwrap_or_default();
    let org = payload["organizationId"].as_str();
    let pinned = app.linear.surface.pinned_id();
    if !admits(pinned.as_deref(), org) {
        tracing::warn!(
            "refused Linear webhook {event} from organization {} (pinned: {})",
            org.unwrap_or("none"),
            pinned.as_deref().unwrap_or("none")
        );
        return StatusCode::FORBIDDEN;
    }
    // Linear wants a 200 within 5 seconds; the work happens in the background.
    tokio::spawn(handle(app, payload));
    StatusCode::OK
}

/// Runs only for webhooks `admits` let through, so issue changes in another workspace never
/// stop sessions or remove worktrees here.
async fn handle(app: Arc<App>, p: Value) {
    if let Some(change) = issue_change(&p) {
        on_issue_change(&app, change).await;
        return;
    }
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
    let (prompt, request) = match p["action"].as_str() {
        Some("created") => {
            // A delegated session starts work on the issue. A mention may be just a question,
            // so the agent moves those issues itself when it starts work (see the prompt).
            if mention_request(&p).is_none()
                && let Some(issue_id) = issue["id"].as_str()
            {
                let (app, sid, issue_id) = (app.clone(), sid.to_string(), issue_id.to_string());
                tokio::spawn(async move {
                    if let Err(e) = app.linear.surface.start_issue(&app, &issue_id).await {
                        tracing::warn!("[{sid}] moving issue to started failed: {e:#}");
                    }
                });
            }
            (
                initial_prompt(&p),
                p["agentSession"]["comment"]["body"].as_str(),
            )
        }
        Some("prompted") if p["agentActivity"]["signal"] == "stop" => {
            if !app.linear.stop(sid, None) {
                app.linear
                    .surface
                    .finish(&app, sid, Vec::new(), Outcome::Stopped(None))
                    .await;
            }
            return;
        }
        Some("prompted") => {
            let body = p["agentActivity"]["content"]["body"].as_str();
            (body.unwrap_or_default().to_string(), body)
        }
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
        rec.prompted_at = crate::store::now_secs();
        rec.closed = false;
    });
    let linear = &app.linear.surface;
    if let Err(unrouted) = linear.choose_repo(&app, sid, issue, request).await {
        // Keep Linear's issue context for the turn that runs once the repository is known.
        app.store.update(|s| {
            if let Some(rec) = s.sessions.get_mut(sid) {
                rec.pending_prompt.get_or_insert(prompt);
            }
        });
        let content = match unrouted {
            Unrouted::Ask(question) => {
                tracing::info!("[{sid}] no repository: {question}");
                json!({ "type": "elicitation", "body": question })
            }
            Unrouted::Stuck(problem) => {
                tracing::warn!("[{sid}] repository unavailable: {problem}");
                json!({ "type": "error", "body": problem })
            }
        };
        linear.activity(&app, sid, content, false).await;
        return;
    }
    let pending = app.store.update(|s| {
        s.sessions
            .get_mut(sid)
            .and_then(|rec| rec.pending_prompt.take())
    });
    let prompt = match pending {
        Some(first) => format!("{first}\n\n{prompt}"),
        None => prompt,
    };
    app.linear.submit(&app, sid, prompt, ());
}

/// A change to an issue that ends the agent's work on it.
#[derive(Debug, PartialEq)]
enum IssueChange<'a> {
    /// Moved to a completed or canceled state, or deleted. Holds the issue id.
    Closed(&'a str),
    /// The app was unassigned or undelegated. Holds the issue id.
    Unassigned(&'a str),
}

/// Classifies data-change and app notification webhooks. A state change is recognised by
/// `updatedFrom.stateId`, so later edits to an already closed issue do not count.
/// `issueStatusChanged` notifications are left out: the `Issue` update carries the same change.
/// An empty id is ignored: Zulip sessions record none and must never match.
fn issue_change(p: &Value) -> Option<IssueChange<'_>> {
    fn id(v: &Value) -> Option<&str> {
        v.as_str().filter(|id| !id.is_empty())
    }
    let data = &p["data"];
    match (p["type"].as_str()?, p["action"].as_str()?) {
        ("Issue", "remove") => id(&data["id"]).map(IssueChange::Closed),
        ("Issue", "update") => {
            let trashed = data["trashed"] == true;
            let closed = p["updatedFrom"].get("stateId").is_some()
                && matches!(
                    data["state"]["type"].as_str(),
                    Some("completed" | "canceled")
                );
            if trashed || closed {
                id(&data["id"]).map(IssueChange::Closed)
            } else {
                None
            }
        }
        ("AppUserNotification", "issueUnassignedFromYou") => {
            let n = &p["notification"];
            id(&n["issueId"])
                .or_else(|| id(&n["issue"]["id"]))
                .map(IssueChange::Unassigned)
        }
        _ => None,
    }
}

/// Stops the issue's running sessions. A closed issue also marks its sessions closed and
/// removes their worktrees (branches stay); one with uncommitted changes is kept and logged.
async fn on_issue_change(app: &Arc<App>, change: IssueChange<'_>) {
    let (issue_id, note) = match change {
        IssueChange::Closed(id) => (id, "The issue was closed, so I stopped working on it."),
        IssueChange::Unassigned(id) => (id, "I was unassigned, so I stopped working on this."),
    };
    let sessions: Vec<(String, SessionRec)> = app.store.read(|s| {
        s.sessions
            .iter()
            .filter(|(_, r)| r.issue_id == issue_id)
            .map(|(k, r)| (k.clone(), r.clone()))
            .collect()
    });
    for (sid, _) in &sessions {
        if app.linear.stop(sid, Some(note.into())) {
            tracing::info!("[{sid}] stopping: {change:?}");
        }
    }
    if matches!(change, IssueChange::Unassigned(_)) {
        return;
    }
    app.store.update(|s| {
        for r in s.sessions.values_mut() {
            if r.issue_id == issue_id {
                r.closed = true;
            }
        }
    });
    let workspaces = app
        .store
        .read(|s| removable_workspaces(&s.sessions, issue_id));
    for (workspace, (sid, recorded)) in workspaces {
        let main_clone = match recorded {
            Some(_) => None,
            None => worktree::main_clone(&workspace).await.ok(),
        };
        let repo = match cleanup_repo(
            &app.cfg.repos,
            &app.cfg.worktrees_dir,
            recorded.as_deref(),
            &workspace,
            main_clone.as_deref(),
        ) {
            Ok(repo) => repo,
            Err(why) => {
                tracing::info!("[{sid}] keeping {}: {why}", workspace.display());
                continue;
            }
        };
        // Waits for a stopped turn to release the worktree. A prompt may have reopened the
        // issue, or another issue's session taken the worktree, in the meantime.
        let _guard = app.lock_workspace(&workspace).await;
        if !app
            .store
            .read(|s| removable_workspaces(&s.sessions, issue_id).contains_key(&workspace))
        {
            tracing::info!(
                "[{sid}] keeping worktree {}: an open session uses it again",
                workspace.display()
            );
            continue;
        }
        if workspace.exists() {
            match worktree::remove(&repo.path, &workspace).await {
                Ok(true) => tracing::info!(
                    "[{sid}] removed worktree {} from {}",
                    workspace.display(),
                    repo.name
                ),
                Ok(false) => {
                    tracing::warn!(
                        "[{sid}] keeping worktree {}: it has uncommitted changes",
                        workspace.display()
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!("[{sid}] removing worktree {}: {e:#}", workspace.display());
                    continue;
                }
            }
        }
        app.store.update(|s| {
            for r in s.sessions.values_mut() {
                if r.issue_id == issue_id && r.workspace.as_ref() == Some(&workspace) {
                    r.workspace = None;
                }
            }
        });
    }
}

/// The closed issue's workspaces that may go, each with the session that recorded it first
/// and its repository. Sessions of one issue share its worktree, but a stacked branch can
/// put another issue's session in it too, so a workspace any open session records stays, and
/// nothing goes while one of the issue's own sessions has been reopened.
fn removable_workspaces(
    sessions: &HashMap<String, SessionRec>,
    issue_id: &str,
) -> BTreeMap<PathBuf, (String, Option<String>)> {
    let mut ours: Vec<_> = sessions
        .iter()
        .filter(|(_, r)| r.issue_id == issue_id)
        .collect();
    if ours.iter().any(|(_, r)| !r.closed) {
        return BTreeMap::new();
    }
    ours.sort_by_key(|(sid, _)| *sid);
    let mut workspaces = BTreeMap::new();
    for (sid, rec) in ours {
        if let Some(w) = &rec.workspace {
            workspaces
                .entry(w.clone())
                .or_insert_with(|| (sid.clone(), rec.repo.clone()));
        }
    }
    workspaces.retain(|w, _| {
        !sessions
            .values()
            .any(|r| !r.closed && r.workspace.as_ref() == Some(w))
    });
    workspaces
}

/// The git repository whose main clone removes the closed issue's `workspace`, or why it
/// stays: the session's recorded repository, else (before routing) the one `main_clone`
/// belongs to. A non-git repository's workspace is the directory itself, so it never goes.
fn cleanup_repo<'a>(
    repos: &'a [Repo],
    worktrees_dir: &Path,
    recorded: Option<&str>,
    workspace: &Path,
    main_clone: Option<&Path>,
) -> Result<&'a Repo, String> {
    let repo = match recorded {
        Some(name) => repos::by_name(repos, name)
            .ok_or_else(|| format!("its repository `{name}` is not configured"))?,
        None => existing_repo(repos, main_clone)
            .ok_or_else(|| "it belongs to no configured repository".to_string())?,
    };
    if !repo.git {
        return Err(format!("`{}` is not a git repository", repo.name));
    }
    // `starts_with` compares components, so `..` could climb out of WORKTREES_DIR.
    if workspace.components().any(|c| c == Component::ParentDir) {
        return Err("its path contains `..`".into());
    }
    if workspace == repo.path || !workspace.starts_with(worktrees_dir) {
        return Err("it is not a worktree under WORKTREES_DIR".into());
    }
    Ok(repo)
}

/// Why a session cannot start its turn yet.
enum Unrouted {
    /// Nothing on the issue decides the repository; the question for the requester.
    Ask(String),
    /// The session already belongs to a repository that is not configured; a reply cannot fix
    /// that, the config must.
    Stuck(String),
}

impl Linear {
    /// Settles which repository the session works in, once: later turns keep it.
    async fn choose_repo(
        &self,
        app: &Arc<App>,
        sid: &str,
        issue: &Value,
        request: Option<&str>,
    ) -> Result<(), Unrouted> {
        let (current, workspace) = app.store.read(|s| {
            s.sessions
                .get(sid)
                .map(|r| (r.repo.clone(), r.workspace.clone()))
                .unwrap_or_default()
        });
        if let Some(name) = current {
            // Choosing again would leave the old repository's worktree and Claude session under
            // another repository's prompt and sandbox.
            return match app.cfg.repo(&name) {
                Some(_) => Ok(()),
                None => Err(Unrouted::Stuck(format!(
                    "This session works in `{name}`, which is no longer in repos.json. Add it \
                     back and restart mothership to continue."
                ))),
            };
        }
        if let Some(workspace) = workspace {
            // A session from before repository routing: it stays where its worktree is.
            let main_clone = worktree::main_clone(&workspace).await.ok();
            let Some(repo) = existing_repo(&app.cfg.repos, main_clone.as_deref()) else {
                return Err(Unrouted::Stuck(format!(
                    "This session's worktree {} belongs to no repository in repos.json. Add \
                     its repository and restart mothership to continue.",
                    workspace.display()
                )));
            };
            self.settle(app, sid, repo, "the repository of this session's worktree")
                .await;
            return Ok(());
        }
        let routing = if app.cfg.repos.len() > 1 {
            self.issue_routing(app, issue["id"].as_str().unwrap_or_default())
                .await
                .inspect_err(|e| {
                    tracing::warn!("[{sid}] issue lookup for repository routing failed: {e:#}");
                })
                .ok()
        } else {
            Some(Value::Null)
        };
        let facts = issue_facts(routing.as_ref(), issue, request);
        let choice = repos::select(&app.cfg.repos, &facts).map_err(Unrouted::Ask)?;
        self.settle(app, sid, choice.repo, &choice.reason).await;
        Ok(())
    }

    /// Records the session's repository and tells the requester which one and why.
    async fn settle(&self, app: &Arc<App>, sid: &str, repo: &Repo, reason: &str) {
        let name = repo.name.clone();
        tracing::info!("[{sid}] repository {name} ({reason})");
        app.store.update(|s| {
            if let Some(rec) = s.sessions.get_mut(sid) {
                rec.repo = Some(name.clone());
            }
        });
        let note = format!("Working in `{name}` ({reason}).");
        self.activity(app, sid, thought(&note), false).await;
    }

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

/// The repository of a session that has a worktree but predates routing: the one whose main
/// clone the worktree was cut from, or with a single repository, that one as before.
pub fn existing_repo<'a>(repos: &'a [Repo], main_clone: Option<&Path>) -> Option<&'a Repo> {
    main_clone
        .and_then(|clone| repos::owning(repos, clone))
        .or(match repos {
            [only] => Some(only),
            _ => None,
        })
}

/// What routing looks at: the `issue_routing` result (`None` when the lookup failed), with the
/// webhook's description when the lookup has none. The request (the @mention or reply) comes
/// first, so a reply's `[repo=…]` corrects a wrong one in the description.
fn issue_facts(routing: Option<&Value>, issue: &Value, request: Option<&str>) -> IssueFacts {
    let mut facts = routing.map(routing_facts).unwrap_or_default();
    facts.lookup_failed = routing.is_none();
    if facts.texts.is_empty()
        && let Some(description) = issue["description"].as_str()
    {
        facts.texts.push(description.to_string());
    }
    if let Some(request) = request {
        facts.texts.insert(0, request.to_string());
    }
    facts
}

/// Repository routing facts from an `issue_routing` result (`Null` when unavailable).
fn routing_facts(issue: &Value) -> IssueFacts {
    let entity = |v: &Value, aliases: &[&str]| {
        v["name"].as_str().map(|name| Ref {
            name: name.to_string(),
            aliases: aliases
                .iter()
                .filter_map(|k| v[k].as_str().map(String::from))
                .collect(),
        })
    };
    IssueFacts {
        texts: issue["description"]
            .as_str()
            .map(String::from)
            .into_iter()
            .collect(),
        labels: issue["labels"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| entity(l, &["id"]))
            .collect(),
        project: entity(&issue["project"], &["id", "slugId"]),
        team: entity(&issue["team"], &["id", "key"]),
        lookup_failed: false,
    }
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
    if let Some(comment) = mention_request(p) {
        prompt.push_str("\n\nRequest:\n");
        prompt.push_str(comment);
    }
    prompt
}

/// The @mention that started the session, if it was one. A delegated session also carries a
/// comment, Linear's own thread marker, which is not a request.
fn mention_request(p: &Value) -> Option<&str> {
    p["agentSession"]["comment"]["body"]
        .as_str()
        .filter(|c| !c.is_empty() && !c.contains("This thread is for an agent session"))
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
        system_prompt.push_str(
            &repo.instructions(
                app.cfg
                    .extra_prompt
                    .as_deref()
                    .unwrap_or(app.cfg.review.instructions()),
            ),
        );

        let plugin_dirs = app.cfg.plugin_dirs();
        let mut readable = vec![workspace.clone(), repo.path.clone()];
        readable.extend(plugin_dirs.iter().cloned());
        readable.extend(app.github.as_ref().map(|g| g.bin_dir.clone()));
        let mut mcp_configs = vec![app.linear_mcp_config(key)?];
        mcp_configs.extend(app.cfg.mcp_configs.iter().cloned());
        mcp_configs.extend(repo.mcp_configs.iter().cloned());
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
            Outcome::Stopped(note) => json!({
                "type": "response",
                "body": note.as_deref().unwrap_or("Stopped."),
            }),
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
    let tokens = match linear.exchange_code(&app, code).await {
        Ok(tokens) => tokens,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("token exchange failed: {e:#}"),
            );
        }
    };
    let ws = match api::workspace_of(&app, &tokens.access_token).await {
        Ok(ws) => ws,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("workspace lookup failed: {e:#}"),
            );
        }
    };
    // The token is stored only once its workspace is the one this instance serves.
    if let Err(e) = linear.pin(&app, ws) {
        // The details name workspaces, so they stay in the log, not the public response.
        tracing::warn!("refused Linear install: {e:#}");
        return (
            StatusCode::FORBIDDEN,
            "this instance serves another Linear workspace".into(),
        );
    }
    app.store.update(|s| s.linear = tokens);
    (
        StatusCode::OK,
        "Linear authorized. You can close this tab.".into(),
    )
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
    fn mention_is_told_apart_from_delegation() {
        let with = |body: &str| json!({"agentSession": {"comment": {"body": body}}});
        assert_eq!(mention_request(&with("@bot fix it")), Some("@bot fix it"));
        assert_eq!(
            mention_request(&with("This thread is for an agent session with Bot.")),
            None
        );
        assert_eq!(mention_request(&with("")), None);
        assert_eq!(mention_request(&json!({"agentSession": {}})), None);
    }

    #[test]
    fn mention_prompt_includes_request() {
        let p = json!({"agentSession":{"issue":{"identifier":"A-1","title":"T","description":"D"},"comment":{"body":"@bot fix it"}}});
        assert_eq!(initial_prompt(&p), "A-1: T\n\nD\n\nRequest:\n@bot fix it");
        let p = json!({"promptContext":"<issue/>","agentSession":{}});
        assert_eq!(initial_prompt(&p), "<issue/>");
    }

    #[test]
    fn routing_facts_carry_names_and_aliases() {
        let issue = json!({
            "description": "Do it [repo=app]",
            "project": {"id": "p1", "name": "Launch", "slugId": "abc123"},
            "team": {"id": "t1", "key": "EN", "name": "Engineering"},
            "labels": {"nodes": [{"id": "l1", "name": "backend"}]},
        });
        let facts = routing_facts(&issue);
        assert_eq!(facts.texts, ["Do it [repo=app]"]);
        let project = facts.project.unwrap();
        assert_eq!(
            (project.name.as_str(), project.aliases),
            ("Launch", vec!["p1".into(), "abc123".into()])
        );
        let team = facts.team.unwrap();
        assert_eq!(
            (team.name.as_str(), team.aliases),
            ("Engineering", vec!["t1".into(), "EN".into()])
        );
        assert_eq!(facts.labels[0].aliases, ["l1"]);

        let none = routing_facts(&json!({"project": null, "labels": {"nodes": []}}));
        assert!(none.texts.is_empty() && none.labels.is_empty() && none.project.is_none());
        assert!(routing_facts(&Value::Null).team.is_none());
    }

    #[test]
    fn request_is_searched_before_the_description() {
        let issue = json!({"description": "webhook copy"});
        let looked_up = json!({"description": "Fix it [repo=nope]"});
        let facts = issue_facts(Some(&looked_up), &issue, Some("[repo=app]"));
        assert_eq!(facts.texts, ["[repo=app]", "Fix it [repo=nope]"]);
        assert!(!facts.lookup_failed);

        let facts = issue_facts(None, &issue, None);
        assert_eq!(facts.texts, ["webhook copy"]);
        assert!(facts.lookup_failed);
    }

    #[test]
    fn existing_worktree_keeps_its_repository() {
        let app = Repo::single("/src/app".into(), "main".into());
        let other = Repo::single("/src/other".into(), "main".into());
        let two = [app, other];
        let pick = |repos: &[Repo], clone: Option<&str>| {
            existing_repo(repos, clone.map(Path::new)).map(|r| r.name.clone())
        };
        assert_eq!(pick(&two, Some("/src/other")), Some("other".into()));
        assert_eq!(pick(&two, Some("/src/gone")), None);
        assert_eq!(pick(&two, None), None);
        assert_eq!(
            pick(&two[..1], None),
            Some("app".into()),
            "single repo as before"
        );
        assert_eq!(pick(&two[..1], Some("/src/gone")), Some("app".into()));
    }

    #[test]
    fn closed_issue_worktrees_go_from_their_own_repository() {
        let app = Repo::single("/src/app".into(), "main".into());
        let vault = Repo {
            git: false,
            ..Repo::single("/notes/vault".into(), "main".into())
        };
        let repos = [app, Repo::single("/src/lib".into(), "main".into()), vault];
        let wt = Path::new("/wt");
        let pick = |recorded: Option<&str>, workspace: &str, clone: Option<&str>| {
            cleanup_repo(
                &repos,
                wt,
                recorded,
                Path::new(workspace),
                clone.map(Path::new),
            )
            .map(|r| r.name.clone())
        };
        assert_eq!(pick(Some("lib"), "/wt/SH-1", None), Ok("lib".into()));
        assert_eq!(
            pick(None, "/wt/SH-1", Some("/src/app")),
            Ok("app".into()),
            "before routing: the worktree's main clone"
        );
        assert!(pick(None, "/wt/SH-1", Some("/src/gone")).is_err());
        assert!(pick(Some("gone"), "/wt/SH-1", None).is_err());
        assert!(
            pick(Some("vault"), "/notes/vault", None).is_err(),
            "a non-git repository is the workspace itself"
        );
        assert!(pick(Some("vault"), "/wt/SH-1", None).is_err());
        assert!(pick(Some("app"), "/src/app", None).is_err());
        assert!(pick(Some("app"), "/elsewhere/SH-1", None).is_err());
        assert!(pick(Some("app"), "/wt/../src/app", None).is_err());
    }

    #[test]
    fn closed_issue_keeps_worktrees_open_sessions_use() {
        let rec = |issue: &str, workspace: Option<&str>, closed| SessionRec {
            issue_id: issue.into(),
            workspace: workspace.map(PathBuf::from),
            repo: Some("app".into()),
            closed,
            ..SessionRec::default()
        };
        let sessions = HashMap::from([
            ("a1".to_string(), rec("i9", Some("/wt/SH-9"), true)),
            ("a2".to_string(), rec("i9", Some("/wt/SH-9b"), true)),
            ("b1".to_string(), rec("i10", Some("/wt/SH-9"), false)),
            ("c1".to_string(), rec("i11", Some("/wt/SH-9b"), true)),
            ("chat".to_string(), rec("", None, false)),
        ]);
        let removable = removable_workspaces(&sessions, "i9");
        assert_eq!(
            removable.keys().collect::<Vec<_>>(),
            [Path::new("/wt/SH-9b")],
            "SH-9 stays: i10's open session on a stacked branch records it"
        );
        assert_eq!(removable[Path::new("/wt/SH-9b")].0, "a2");

        let mut reopened = sessions;
        reopened.insert("a3".into(), rec("i9", None, false));
        assert!(
            removable_workspaces(&reopened, "i9").is_empty(),
            "a reopened session of the issue keeps everything"
        );
    }

    #[test]
    fn classifies_issue_changes() {
        let update = |state: &str, updated_from: Value| json!({"type":"Issue","action":"update","data":{"id":"i1","state":{"type":state}},"updatedFrom":updated_from});
        let moved = json!({"stateId":"s0"});
        assert_eq!(
            issue_change(&update("completed", moved.clone())),
            Some(IssueChange::Closed("i1"))
        );
        assert_eq!(
            issue_change(&update("canceled", moved.clone())),
            Some(IssueChange::Closed("i1"))
        );
        assert_eq!(issue_change(&update("started", moved)), None);
        assert_eq!(
            issue_change(&update("completed", json!({"title":"old"}))),
            None,
            "edit to an already closed issue"
        );
        assert_eq!(
            issue_change(
                &json!({"type":"Issue","action":"update","data":{"id":"i1","trashed":true,"state":{"type":"started"}},"updatedFrom":{"trashed":null}})
            ),
            Some(IssueChange::Closed("i1"))
        );
        assert_eq!(
            issue_change(&json!({"type":"Issue","action":"remove","data":{"id":"i1"}})),
            Some(IssueChange::Closed("i1"))
        );
        assert_eq!(
            issue_change(
                &json!({"type":"AppUserNotification","action":"issueUnassignedFromYou","notification":{"issueId":"i1"}})
            ),
            Some(IssueChange::Unassigned("i1"))
        );
        assert_eq!(
            issue_change(
                &json!({"type":"AppUserNotification","action":"issueUnassignedFromYou","notification":{"issue":{"id":"i1"}}})
            ),
            Some(IssueChange::Unassigned("i1"))
        );
        for ignored in [
            json!({"type":"AppUserNotification","action":"issueStatusChanged","notification":{"issueId":"i1"}}),
            json!({"type":"AppUserNotification","action":"issueNewComment","notification":{"issueId":"i1"}}),
            json!({"type":"Issue","action":"create","data":{"id":"i1"}}),
            json!({"type":"Issue","action":"remove","data":{"id":""}}),
            json!({"type":"AppUserNotification","action":"issueUnassignedFromYou","notification":{"issueId":"","issue":{"id":""}}}),
            json!({"type":"AgentSessionEvent","action":"created"}),
        ] {
            assert_eq!(issue_change(&ignored), None, "{ignored}");
        }
    }

    #[test]
    fn webhooks_only_from_the_pinned_workspace() {
        assert!(admits(Some("org-1"), Some("org-1")));
        assert!(!admits(Some("org-1"), Some("org-2")));
        assert!(!admits(Some("org-1"), None));
        assert!(!admits(None, Some("org-1")), "unpinned");
        assert!(!admits(None, None), "unpinned, no organization");
    }

    #[test]
    fn pin_checks_setting_and_existing_pin() {
        let ws = |id: &str, url_key: &str| Workspace {
            id: id.into(),
            url_key: url_key.into(),
            name: url_key.to_uppercase(),
        };
        let alean = ws("org-1", "alean");
        let personal = ws("org-2", "personal");
        assert!(
            check_pin(None, None, &alean).is_ok(),
            "first install, no setting"
        );
        assert!(check_pin(Some("alean"), None, &alean).is_ok());
        assert!(check_pin(Some("org-1"), None, &alean).is_ok());
        assert!(check_pin(Some("personal"), None, &alean).is_err());
        assert!(check_pin(None, Some(&alean), &alean).is_ok(), "same pin");
        assert!(check_pin(None, Some(&alean), &personal).is_err());
        assert!(
            check_pin(Some("personal"), Some(&alean), &personal).is_err(),
            "pin never switches"
        );
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
