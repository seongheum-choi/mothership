use crate::{App, Res, SessionRec};
use serde_json::{Value, json};
use std::{os::unix::fs::OpenOptionsExt, path::Path, process::Stdio, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc,
};

pub enum Msg {
    Prompt(String),
    Stop,
}

/// Entry point for a verified Linear webhook.
pub async fn handle(app: Arc<App>, p: Value) {
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
    match p["action"].as_str() {
        Some("created") => submit(&app, sid, issue, initial_prompt(&p)).await,
        Some("prompted") if p["agentActivity"]["signal"] == "stop" => stop(&app, sid).await,
        Some("prompted") => {
            let body = p["agentActivity"]["content"]["body"]
                .as_str()
                .unwrap_or_default();
            submit(&app, sid, issue, body.to_string()).await
        }
        _ => {}
    }
}

/// Linear's own `promptContext` (issue, comments, guidance) when present, else a minimal one.
fn initial_prompt(p: &Value) -> String {
    if let Some(ctx) = p["promptContext"].as_str() {
        return ctx.to_string();
    }
    let issue = &p["agentSession"]["issue"];
    let mut s = format!(
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
        s.push_str("\n\nRequest:\n");
        s.push_str(comment);
    }
    s
}

async fn submit(app: &Arc<App>, sid: &str, issue: &Value, prompt: String) {
    let field = |k: &str| issue[k].as_str().unwrap_or_default().to_string();
    app.update(|s| {
        s.sessions
            .entry(sid.to_string())
            .or_insert_with(|| SessionRec {
                issue_id: field("id"),
                identifier: field("identifier"),
                title: field("title"),
                url: field("url"),
                worktree: None,
                branch: None,
                claude_session_id: None,
            });
    });
    // Senders only send while holding `live`, and a worker only exits after seeing an empty
    // queue under the same lock, so no prompt can land in a channel nobody reads.
    let mut live = app.live.lock().unwrap();
    if let Some(tx) = live.get(sid) {
        let _ = tx.send(Msg::Prompt(prompt));
        return;
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let _ = tx.send(Msg::Prompt(prompt));
    live.insert(sid.to_string(), tx);
    tokio::spawn(worker(app.clone(), sid.to_string(), rx));
}

async fn stop(app: &Arc<App>, sid: &str) {
    let tx = app.live.lock().unwrap().get(sid).cloned();
    match tx {
        Some(tx) => {
            let _ = tx.send(Msg::Stop);
        }
        None => {
            app.activity(
                sid,
                json!({ "type": "response", "body": "Stopped." }),
                false,
            )
            .await
        }
    }
}

/// One worker per busy agent session: runs turns one after another, folding prompts that
/// arrive mid-turn into the next turn.
// ponytail: mid-turn prompts wait for the turn to end; stream them into stdin (--input-format stream-json) if that lag hurts.
async fn worker(app: Arc<App>, sid: String, mut rx: mpsc::UnboundedReceiver<Msg>) {
    let mut queued: Vec<String> = Vec::new();
    loop {
        if queued.is_empty() {
            let msg = {
                let mut live = app.live.lock().unwrap();
                match rx.try_recv() {
                    Ok(msg) => msg,
                    Err(_) => {
                        live.remove(&sid);
                        return;
                    }
                }
            };
            match msg {
                Msg::Prompt(p) => queued.push(p),
                Msg::Stop => {
                    app.activity(
                        &sid,
                        json!({ "type": "response", "body": "Stopped." }),
                        false,
                    )
                    .await;
                    continue;
                }
            }
        }
        let prompt = std::mem::take(&mut queued).join("\n\n");
        if let Err(e) = run_turn(&app, &sid, prompt, &mut rx, &mut queued).await {
            eprintln!("[{sid}] {e}");
            app.activity(
                &sid,
                json!({ "type": "error", "body": e.to_string() }),
                false,
            )
            .await;
        }
    }
}

async fn run_turn(
    app: &App,
    sid: &str,
    prompt: String,
    rx: &mut mpsc::UnboundedReceiver<Msg>,
    queued: &mut Vec<String>,
) -> Res<()> {
    // Also the acknowledgement Linear expects within 10 seconds of session creation.
    app.activity(
        sid,
        json!({ "type": "thought", "body": "Working on it…" }),
        true,
    )
    .await;
    let rec = ensure_worktree(app, sid).await?;
    let worktree = rec.worktree.clone().expect("set by ensure_worktree");

    let mut cmd = Command::new(&app.cfg.claude_bin);
    cmd.args(["-p", "--output-format", "stream-json", "--verbose"])
        .args([
            "--permission-mode",
            "bypassPermissions",
            "--strict-mcp-config",
        ])
        .args([
            "--model",
            &app.cfg.model,
            "--fallback-model",
            &app.cfg.fallback_model,
        ])
        .arg("--mcp-config")
        .arg(write_linear_mcp_config(app)?)
        .args(&app.cfg.mcp_configs)
        .arg("--append-system-prompt")
        .arg(system_prompt(app, &rec));
    if let Some(id) = &rec.claude_session_id {
        cmd.args(["--resume", id]);
    }
    let mut child = cmd
        .current_dir(&worktree)
        .env_remove("CLAUDECODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawning {}: {e}", app.cfg.claude_bin))?;
    // Prompt over stdin: no argv length limit, and it stays out of `ps`.
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(prompt.as_bytes()).await?;
    drop(stdin);

    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut relay = Relay::default();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if v["type"] == "system" && v["subtype"] == "init"
                    && let Some(id) = v["session_id"].as_str() {
                        app.update(|s| s.sessions.get_mut(sid).map(|r| r.claude_session_id = Some(id.to_string())));
                    }
                for (content, ephemeral) in relay.on(&v) {
                    app.activity(sid, content, ephemeral).await;
                }
            }
            Some(msg) = rx.recv() => match msg {
                Msg::Prompt(p) => {
                    queued.push(p);
                    app.activity(sid, json!({ "type": "thought", "body": "Noted, I'll pick this up when the current step finishes." }), true).await;
                }
                Msg::Stop => {
                    child.kill().await.ok();
                    queued.clear();
                    app.activity(sid, json!({ "type": "response", "body": "Stopped." }), false).await;
                    return Ok(());
                }
            },
        }
    }
    let status = child.wait().await?;
    if !relay.finished {
        return Err(format!("claude exited ({status}) without a result").into());
    }
    Ok(())
}

async fn ensure_worktree(app: &App, sid: &str) -> Res<SessionRec> {
    let rec = app.store.lock().unwrap().sessions[sid].clone();
    if rec.worktree.as_ref().is_some_and(|w| w.exists()) {
        return Ok(rec);
    }
    let branch = match app.branch_name(&rec.issue_id).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[{sid}] branchName lookup failed ({e}), using identifier");
            rec.identifier.to_lowercase()
        }
    };
    let path = app.cfg.worktrees_dir.join(&rec.identifier);
    if !path.exists() {
        let repo = &app.cfg.repo;
        if let Err(e) = git(repo, &["fetch", "origin"]).await {
            eprintln!("[{sid}] git fetch failed, using local refs: {e}");
        }
        let p = path.to_str().ok_or("worktree path is not UTF-8")?;
        if git(
            repo,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .await
        .is_ok()
        {
            git(repo, &["worktree", "add", p, &branch]).await?;
        } else {
            let base = format!("origin/{}", app.cfg.base_branch);
            git(repo, &["worktree", "add", "-b", &branch, p, &base]).await?;
        }
    }
    Ok(app.update(|s| {
        let r = s.sessions.get_mut(sid).expect("inserted by submit");
        r.worktree = Some(path);
        r.branch = Some(branch);
        r.clone()
    }))
}

async fn git(repo: &Path, args: &[&str]) -> Res<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(())
}

/// Linear's hosted MCP with the app token, rewritten per turn so it carries the current token.
// ponytail: a turn longer than the 24h token lifetime loses Linear MCP mid-turn; proxy MCP through mothership if that ever happens.
fn write_linear_mcp_config(app: &App) -> Res<std::path::PathBuf> {
    let token = app.store.lock().unwrap().linear.access_token.clone();
    let config = json!({ "mcpServers": { "linear": {
        "type": "http",
        "url": "https://mcp.linear.app/mcp",
        "headers": { "Authorization": format!("Bearer {token}") },
    }}});
    let path = app.cfg.home.join("mcp-linear.json");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    serde_json::to_writer(&mut file, &config)?;
    Ok(path)
}

fn system_prompt(app: &App, rec: &SessionRec) -> String {
    let mut s = format!(
        "You are working on Linear issue {} \"{}\" ({}) in a git worktree on branch `{}`. \
         Everything you write is relayed to the Linear agent session.",
        rec.identifier,
        rec.title,
        rec.url,
        rec.branch.as_deref().unwrap_or_default()
    );
    s.push_str("\n\n");
    s.push_str(app.cfg.extra_prompt.as_deref().unwrap_or(
        "When the change is done: commit, push the branch, open a pull request with `gh pr create`, \
         and put the GitHub pull request URL in your final reply.",
    ));
    s
}

/// Turns Claude Code stream-json lines into Linear agent activities.
#[derive(Default)]
struct Relay {
    /// Latest assistant text, held back because the final one repeats as the result.
    pending: Option<String>,
    finished: bool,
}

impl Relay {
    fn on(&mut self, v: &Value) -> Vec<(Value, bool)> {
        let mut out = Vec::new();
        match v["type"].as_str() {
            Some("assistant") => {
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            let text = block["text"].as_str().unwrap_or_default().trim();
                            if !text.is_empty() {
                                self.flush(&mut out);
                                self.pending = Some(text.to_string());
                            }
                        }
                        Some("tool_use") => {
                            self.flush(&mut out);
                            out.push(tool_activity(
                                block["name"].as_str().unwrap_or("Tool"),
                                &block["input"],
                            ));
                        }
                        _ => {}
                    }
                }
            }
            Some("result") => {
                self.finished = true;
                let text = v["result"].as_str().unwrap_or_default().trim().to_string();
                if self.pending.as_deref() == Some(text.as_str()) {
                    self.pending = None;
                }
                self.flush(&mut out);
                if v["is_error"] == true || v["subtype"] != "success" {
                    let body = if text.is_empty() {
                        format!("Claude run failed: {}", v["subtype"])
                    } else {
                        text
                    };
                    out.push((json!({ "type": "error", "body": body }), false));
                } else {
                    out.push((json!({ "type": "response", "body": text }), false));
                }
            }
            _ => {}
        }
        out
    }

    fn flush(&mut self, out: &mut Vec<(Value, bool)>) {
        if let Some(text) = self.pending.take() {
            out.push((json!({ "type": "thought", "body": text }), false));
        }
    }
}

/// TodoWrite becomes a persistent checklist; every other tool call is an ephemeral action.
fn tool_activity(name: &str, input: &Value) -> (Value, bool) {
    if name == "TodoWrite" {
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
        return (json!({ "type": "thought", "body": body }), false);
    }
    let parameter = [
        "command",
        "file_path",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_maps_stream_to_activities() {
        let mut r = Relay::default();
        let lines = [
            json!({"type":"system","subtype":"init","session_id":"x"}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"Looking around."}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"Done."}]}}),
            json!({"type":"result","subtype":"success","is_error":false,"result":"Done."}),
        ];
        let got: Vec<(Value, bool)> = lines.iter().flat_map(|l| r.on(l)).collect();
        assert_eq!(
            got,
            vec![
                (json!({"type":"thought","body":"Looking around."}), false),
                (
                    json!({"type":"action","action":"Bash","parameter":"ls"}),
                    true
                ),
                (json!({"type":"response","body":"Done."}), false),
            ]
        );
        assert!(r.finished);
    }

    #[test]
    fn mention_prompt_includes_request() {
        let p = json!({"agentSession":{"issue":{"identifier":"A-1","title":"T","description":"D"},"comment":{"body":"@bot fix it"}}});
        assert_eq!(initial_prompt(&p), "A-1: T\n\nD\n\nRequest:\n@bot fix it");
        let p = json!({"promptContext":"<issue/>","agentSession":{}});
        assert_eq!(initial_prompt(&p), "<issue/>");
    }
}
