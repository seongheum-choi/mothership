//! The scenarios: each starts its own mothership and drives it through webhooks.

use crate::{
    fake::{HOLD, RELEASE},
    harness::{Ctx, Harness, Setup, ZULIP_TOKEN, eventually, exited, git},
    mock::{self, ORG},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};

pub type Scenario = fn(Ctx) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>;

macro_rules! scenarios {
    ($($name:ident),* $(,)?) => {
        &[$((stringify!($name), (|ctx| Box::pin($name(ctx))) as Scenario)),*]
    };
}

pub const ALL: &[(&str, Scenario)] = scenarios![
    linear_session_gets_a_response,
    linear_agent_is_launched_in_the_sandbox,
    linear_mention_leaves_the_issue_state_alone,
    linear_mode_from_labels_shapes_the_agent,
    linear_mode_conflict_or_broken_file_holds_the_turn,
    linear_closed_issue_cleans_up_its_worktree,
    linear_closed_issue_keeps_worktrees_still_in_use,
    linear_refuses_other_workspaces_and_bad_signatures,
    linear_token_is_refreshed_at_startup,
    linear_routes_between_two_repositories,
    linear_prompt_joins_the_running_turn,
    linear_stop_kills_the_agent,
    github_feedback_continues_the_session,
    github_fork_conversation_comment_is_ignored,
    zulip_mention_gets_a_reply,
    sigterm_stops_running_agents,
];

/// An issue as Linear's API returns it; the webhook carries a subset.
fn issue(n: u32, description: &str) -> Value {
    json!({
        "id": format!("issue-{n}"),
        "identifier": format!("SH-{n}"),
        "title": format!("Issue {n}"),
        "url": format!("https://linear.app/acme/issue/SH-{n}"),
        "description": description,
        "branchName": format!("nate/sh-{n}"),
        "state": {"type": "unstarted"},
        "team": {
            "id": "team-1", "key": "SH", "name": "Shop",
            "states": {"nodes": [{"id": "state-started", "type": "started", "position": 1.0}]},
        },
        "labels": {"nodes": []},
        "project": null,
    })
}

/// `issue(n, …)` with Linear labels of these names.
fn labelled(n: u32, description: &str, labels: &[&str]) -> Value {
    let mut issue = issue(n, description);
    let nodes: Vec<Value> = labels
        .iter()
        .map(|name| json!({"id": format!("label-{name}"), "name": name}))
        .collect();
    issue["labels"] = json!({ "nodes": nodes });
    issue
}

/// Linear's thread marker on a delegated session's comment, which is not a request.
const DELEGATION_MARKER: &str = "This thread is for an agent session with mothership.";

fn session(sid: &str, issue: &Value, comment: &str) -> Value {
    let field = |k: &str| issue[k].clone();
    json!({
        "id": sid,
        "issue": {
            "id": field("id"), "identifier": field("identifier"), "title": field("title"),
            "url": field("url"), "description": field("description"),
        },
        "comment": {"body": comment},
    })
}

/// A delegation: Linear's `created` event with `context` as its prompt context.
fn created(sid: &str, issue: &Value, context: &str) -> Value {
    json!({
        "type": "AgentSessionEvent", "action": "created", "organizationId": ORG,
        "agentSession": session(sid, issue, DELEGATION_MARKER), "promptContext": context,
    })
}

/// An @mention: Linear's `created` event whose comment is the mention itself.
fn mentioned(sid: &str, issue: &Value, comment: &str, context: &str) -> Value {
    json!({
        "type": "AgentSessionEvent", "action": "created", "organizationId": ORG,
        "agentSession": session(sid, issue, comment), "promptContext": context,
    })
}

/// The data-change webhook for an issue moved to a completed state.
fn completed(issue: &Value) -> Value {
    json!({
        "type": "Issue", "action": "update", "organizationId": ORG,
        "data": {"id": issue["id"], "state": {"type": "completed"}},
        "updatedFrom": {"stateId": "state-started"},
    })
}

fn prompted(sid: &str, issue: &Value, body: &str) -> Value {
    json!({
        "type": "AgentSessionEvent", "action": "prompted", "organizationId": ORG,
        "agentSession": session(sid, issue, DELEGATION_MARKER), "agentActivity": {"content": {"body": body}},
    })
}

fn stop(sid: &str, issue: &Value) -> Value {
    json!({
        "type": "AgentSessionEvent", "action": "prompted", "organizationId": ORG,
        "agentSession": session(sid, issue, DELEGATION_MARKER), "agentActivity": {"signal": "stop"},
    })
}

/// Starts a session on a fresh issue and waits until the fake agent holds its first prompt.
/// Returns the issue and the pids of the agent and of the tool it is running.
async fn held_session(h: &Harness, sid: &str, n: u32) -> Result<(Value, u64, u64)> {
    let issue = issue(n, "Hold on.");
    h.mock.add_issue(&issue);
    let context = format!("context of SH-{n} {HOLD}");
    ensure!(h.linear(created(sid, &issue, &context)).await? == 200);
    let call = h.prompt_with(&context).await?;
    let pid = call["pid"].as_u64().context("fake agent pid")?;
    let tool = call["tool_pid"].as_u64().context("fake tool pid")?;
    Ok((issue, pid, tool))
}

fn body(content: &Value) -> &str {
    content["body"].as_str().unwrap_or_default()
}

/// The main clone the worktree at `path` was cut from.
fn main_clone(path: &Path) -> Result<String> {
    let common = git(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok(common.trim_end_matches("/.git").to_string())
}

/// Delegation starts a turn in the issue's worktree, on Linear's branch name, moves the issue
/// to started, and posts the agent's progress and result as activities.
async fn linear_session_gets_a_response(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let issue = issue(1, "Fix the thing.");
    h.mock.add_issue(&issue);
    ensure!(
        h.linear(created("s1", &issue, "<issue>SH-1</issue>"))
            .await?
            == 200
    );

    let response = h.activity("s1", "response", 1).await?;
    ensure!(body(&response) == "Done: <issue>SH-1</issue>", "{response}");
    let thoughts: Vec<String> = h
        .activities("s1")
        .iter()
        .filter(|a| a["type"] == "thought")
        .map(|a| body(a).to_string())
        .collect();
    ensure!(
        thoughts
            .iter()
            .any(|t| t == "Working in `app` (the only configured repository).")
            && thoughts.iter().any(|t| t == "Working on it…")
            && thoughts.iter().any(|t| t == "Looking into it."),
        "{thoughts:?}"
    );
    ensure!(
        h.activities("s1")
            .iter()
            .any(|a| a["type"] == "action" && a["action"] == "Bash"),
        "tool call shown as an action"
    );

    let call = h.prompt_with("<issue>SH-1</issue>").await?;
    let cwd = Path::new(call["cwd"].as_str().context("cwd")?);
    ensure!(
        cwd == h.home.join(".mothership/worktrees/SH-1"),
        "{}",
        cwd.display()
    );
    ensure!(git(cwd, &["symbolic-ref", "--short", "HEAD"])? == "nate/sh-1");
    ensure!(main_clone(cwd)? == h.home.join("src/app").display().to_string());
    ensure!(call["surface"] == "linear" && call["resume"].is_null());

    let update = eventually("issueUpdate", || {
        h.mock.read(|r| r.issue_updates.first().cloned())
    })
    .await?;
    ensure!(
        update == json!({"id": "issue-1", "stateId": "state-started"}),
        "{update}"
    );

    // The next prompt resumes the Claude session the first turn started.
    ensure!(h.linear(prompted("s1", &issue, "And the tests?")).await? == 200);
    let second = h.prompt_with("And the tests?").await?;
    ensure!(
        second["resume"] == format!("fake-{}", call["pid"]),
        "{second}"
    );
    h.activity("s1", "response", 2).await?;
    let updates = h.mock.read(|r| r.issue_updates.clone());
    ensure!(updates.len() == 1, "moved once: {updates:?}");
    Ok(())
}

/// The fake agent's command line for the call `call`.
fn argv(call: &Value) -> Vec<String> {
    call["args"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| a.as_str().unwrap_or_default().to_string())
        .collect()
}

/// The value after the first `name` in `args`.
fn flag<'a>(args: &'a [String], name: &str) -> Result<&'a str> {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].as_str())
        .with_context(|| format!("no {name} in {args:#?}"))
}

/// The deny rules of the `--settings` the agent was started with.
fn deny_rules(args: &[String]) -> Result<Vec<String>> {
    let settings: Value = serde_json::from_str(flag(args, "--settings")?)?;
    serde_json::from_value(settings["permissions"]["deny"].clone())
        .with_context(|| format!("settings without deny rules: {settings}"))
}

/// The sandbox's rule for a directory or a file, as Claude Code spells absolute paths.
fn deny_dir(path: &Path) -> String {
    format!("Read(/{}/**)", path.display())
}

fn deny_file(path: &Path) -> String {
    format!("Read(/{})", path.display())
}

fn thoughts(h: &Harness, sid: &str) -> Vec<String> {
    h.activities(sid)
        .iter()
        .filter(|a| a["type"] == "thought")
        .map(|a| body(a).to_string())
        .collect()
}

/// Without a mode, the agent bypasses permission prompts on the configured models, and its
/// settings deny reading everything in `HOME` but the worktree and the main clone.
async fn linear_agent_is_launched_in_the_sandbox(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let issue = issue(10, "Fix it.");
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s10", &issue, "context of SH-10")).await? == 200);
    h.activity("s10", "response", 1).await?;

    let args = argv(&h.prompt_with("context of SH-10").await?);
    let state = h.home.join(".mothership");
    // `HOME` holds `.mothership` and `src/app`; the worktree is `.mothership/worktrees/SH-10`.
    let settings = json!({"permissions": {"deny": [
        deny_dir(&state.join("mcp")),
        deny_file(&state.join("state.json")),
    ]}});
    let system_prompt = flag(&args, "--append-system-prompt")?;
    ensure!(
        system_prompt
            .starts_with("You are mothership, a Linear agent, working on issue SH-10 \"Issue 10\""),
        "{system_prompt}"
    );
    let mcp_config = state.join("mcp/s10.json").display().to_string();
    let expected = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-mode",
        "bypassPermissions",
        "--strict-mcp-config",
        "--model",
        "opus",
        "--fallback-model",
        "sonnet",
        "--settings",
        &settings.to_string(),
        "--append-system-prompt",
        system_prompt,
        "--mcp-config",
        &mcp_config,
    ];
    ensure!(args == expected, "argv {args:#?}\nexpected {expected:#?}");
    Ok(())
}

/// A mention may be just a question, so its issue stays where it is; a delegation in the same
/// workspace still moves its own.
async fn linear_mention_leaves_the_issue_state_alone(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let asked = issue(11, "What does this do?");
    h.mock.add_issue(&asked);
    let mention = mentioned(
        "s11",
        &asked,
        "@mothership what does this do?",
        "context of SH-11",
    );
    ensure!(h.linear(mention).await? == 200);
    let response = h.activity("s11", "response", 1).await?;
    ensure!(body(&response) == "Done: context of SH-11", "{response}");

    // Its move, if any, would race this one; waiting for this one gives it time to show.
    let delegated = issue(12, "Do it.");
    h.mock.add_issue(&delegated);
    ensure!(
        h.linear(created("s12", &delegated, "context of SH-12"))
            .await?
            == 200
    );
    h.activity("s12", "response", 1).await?;
    eventually("issueUpdate", || {
        h.mock.read(|r| r.issue_updates.first().cloned())
    })
    .await?;
    let updates = h.mock.read(|r| r.issue_updates.clone());
    ensure!(
        updates == [json!({"id": "issue-12", "stateId": "state-started"})],
        "{updates:?}"
    );
    Ok(())
}

/// The issue's label picks a mode: it is announced, sets the permission mode and model, adds
/// its instructions, and adds its deny rules to the sandbox's.
async fn linear_mode_from_labels_shapes_the_agent(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let modes = h.home.join(".mothership/modes");
    std::fs::create_dir_all(&modes)?;
    std::fs::write(
        modes.join("research.md"),
        "---\nlabels: [Research]\nmodel: haiku\npermission_mode: plan\n\
         deny: [Edit, Bash(git push:*)]\n---\nAnswer; change nothing.\n",
    )?;
    std::fs::write(
        modes.join("implement.md"),
        "---\nlabels: [Implement]\nmodel: sonnet\n---\nBuild it.\n",
    )?;
    let issue = labelled(13, "Why is it slow?", &["backend", "research"]);
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s13", &issue, "context of SH-13")).await? == 200);
    h.activity("s13", "response", 1).await?;
    let thoughts = thoughts(&h, "s13");
    ensure!(
        thoughts
            .iter()
            .any(|t| t == "Mode: `research` (label `research`)."),
        "{thoughts:?}"
    );

    let args = argv(&h.prompt_with("context of SH-13").await?);
    ensure!(flag(&args, "--permission-mode")? == "plan", "{args:#?}");
    ensure!(flag(&args, "--model")? == "haiku", "{args:#?}");
    ensure!(flag(&args, "--fallback-model")? == "sonnet", "{args:#?}");
    let state = h.home.join(".mothership");
    let deny = deny_rules(&args)?;
    let expected = [
        deny_dir(&state.join("mcp")),
        deny_dir(&state.join("modes")),
        deny_file(&state.join("state.json")),
        "Edit".to_string(),
        "Bash(git push:*)".to_string(),
    ];
    ensure!(deny == expected, "deny {deny:#?}\nexpected {expected:#?}");
    let system_prompt = flag(&args, "--append-system-prompt")?;
    ensure!(
        system_prompt
            .contains("This session is in the `research` mode:\n\nAnswer; change nothing."),
        "{system_prompt}"
    );
    Ok(())
}

/// Labels that pick two modes get a question and a broken mode file an error; neither session
/// runs a turn without its mode.
async fn linear_mode_conflict_or_broken_file_holds_the_turn(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let modes = h.home.join(".mothership/modes");
    std::fs::create_dir_all(&modes)?;
    std::fs::write(modes.join("a.md"), "---\nlabels: [Alpha]\n---\nA.\n")?;
    std::fs::write(modes.join("b.md"), "---\nlabels: [Beta]\n---\nB.\n")?;
    let both = labelled(14, "Both.", &["Alpha", "Beta"]);
    h.mock.add_issue(&both);
    ensure!(h.linear(created("s14", &both, "context of SH-14")).await? == 200);
    let question = h.activity("s14", "elicitation", 1).await?;
    ensure!(
        body(&question).starts_with(
            "The issue's labels select more than one mode: `a` (label `Alpha`), `b` (label `Beta`)."
        ),
        "{question}"
    );

    std::fs::write(modes.join("c.md"), "---\nlabels: [Gamma]\n")?;
    let broken = labelled(15, "Broken.", &["Gamma"]);
    h.mock.add_issue(&broken);
    ensure!(
        h.linear(created("s15", &broken, "context of SH-15"))
            .await?
            == 200
    );
    let error = h.activity("s15", "error", 1).await?;
    ensure!(
        body(&error).starts_with("A mode file is broken:") && body(&error).contains("c.md"),
        "{error}"
    );

    let calls = h.calls("claude");
    ensure!(calls.is_empty(), "turns ran: {calls:?}");
    for sid in ["s14", "s15"] {
        let activities = h.activities(sid);
        ensure!(
            !activities.iter().any(|a| a["type"] == "response"),
            "{sid}: {activities:?}"
        );
    }
    Ok(())
}

/// Delegates `issue` as session `sid` and returns the directory its turn ran in.
async fn worked_on(h: &Harness, sid: &str, issue: &Value) -> Result<PathBuf> {
    h.mock.add_issue(issue);
    let context = format!(
        "context of {}",
        issue["identifier"].as_str().unwrap_or_default()
    );
    ensure!(h.linear(created(sid, issue, &context)).await? == 200);
    h.activity(sid, "response", 1).await?;
    let call = h.prompt_with(&context).await?;
    Ok(PathBuf::from(call["cwd"].as_str().context("cwd")?))
}

/// Closing an issue marks its session closed and removes its worktree, keeping the branch.
async fn linear_closed_issue_cleans_up_its_worktree(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let issue = issue(16, "Finish it.");
    let worktree = worked_on(&h, "s16", &issue).await?;
    ensure!(worktree == h.home.join(".mothership/worktrees/SH-16"));
    let main = h.home.join("src/app");

    ensure!(h.linear(completed(&issue)).await? == 200);
    eventually("the worktree to be removed", || {
        (!worktree.exists()).then_some(())
    })
    .await?;
    let session = eventually("the session to be closed", || {
        let session = h.sessions().ok()?["s16"].take();
        (session["closed"] == true && session["workspace"].is_null()).then_some(session)
    })
    .await?;
    ensure!(session["branch"] == "nate/sh-16", "{session}");
    let worktrees = git(&main, &["worktree", "list", "--porcelain"])?;
    ensure!(!worktrees.contains("SH-16"), "{worktrees}");
    ensure!(
        git(&main, &["branch", "--list", "nate/sh-16"])?.contains("nate/sh-16"),
        "the branch stays"
    );
    Ok(())
}

/// Closing issues keeps a worktree another issue's open session works in, a repository
/// without git, and a worktree with uncommitted changes, while an unused one still goes.
async fn linear_closed_issue_keeps_worktrees_still_in_use(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        repos: vec![("app", "o/app")],
        folders: vec!["notes"],
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    let worktrees = h.home.join(".mothership/worktrees");

    let shared = issue(17, "[repo=app] Base.");
    let shared_dir = worked_on(&h, "s17", &shared).await?;
    // A stacked issue on the same branch reuses the checkout git will not make twice.
    let mut stacked = issue(18, "[repo=app] On top.");
    stacked["branchName"] = json!("nate/sh-17");
    ensure!(worked_on(&h, "s18", &stacked).await? == shared_dir);

    let dirty = issue(19, "[repo=app] Half done.");
    let dirty_dir = worked_on(&h, "s19", &dirty).await?;
    std::fs::write(dirty_dir.join("scratch.txt"), "not committed")?;

    let notes = h.home.join("src/notes");
    std::fs::write(notes.join("note.md"), "kept")?;
    let plain = issue(20, "[repo=notes] Tidy up.");
    ensure!(worked_on(&h, "s20", &plain).await? == notes);

    let unused = issue(21, "[repo=app] Done.");
    let unused_dir = worked_on(&h, "s21", &unused).await?;
    ensure!(unused_dir == worktrees.join("SH-21"));

    for issue in [&shared, &dirty, &plain, &unused] {
        ensure!(h.linear(completed(issue)).await? == 200);
    }
    h.logged(&format!(
        "keeping worktree {}: it has uncommitted changes",
        dirty_dir.display()
    ))
    .await?;
    eventually("the unused worktree to be removed", || {
        (!unused_dir.exists()).then_some(())
    })
    .await?;
    let sessions = eventually("the closed issues' sessions to be closed", || {
        let sessions = h.sessions().ok()?;
        ["s17", "s19", "s20", "s21"]
            .iter()
            .all(|sid| sessions[sid]["closed"] == true)
            .then_some(sessions)
    })
    .await?;

    ensure!(sessions["s18"]["closed"] == false, "{sessions}");
    ensure!(shared_dir == worktrees.join("SH-17") && shared_dir.join(".git").exists());
    ensure!(
        sessions["s17"]["workspace"] == json!(shared_dir),
        "{sessions}"
    );
    ensure!(dirty_dir.join("scratch.txt").exists());
    ensure!(
        sessions["s19"]["workspace"] == json!(dirty_dir),
        "{sessions}"
    );
    ensure!(std::fs::read_to_string(notes.join("note.md"))? == "kept");
    Ok(())
}

/// Webhooks from another workspace get 403; bad, missing or stale signatures get 401.
async fn linear_refuses_other_workspaces_and_bad_signatures(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let issue = issue(2, "x");
    let mut foreign = created("s2", &issue, "foreign");
    foreign["organizationId"] = json!("org-2");
    ensure!(h.linear(foreign).await? == 403);

    let mut payload = created("s2", &issue, "unsigned");
    payload["webhookTimestamp"] = json!(1);
    let body = payload.to_string().into_bytes();
    let stale = crate::harness::sign(crate::harness::LINEAR_SECRET, &body);
    for signature in ["", "zz", &"0".repeat(64), &stale] {
        let status = h
            .post(
                "/linear-webhook",
                &[("linear-signature", signature)],
                body.clone(),
            )
            .await?;
        ensure!(status == 401, "signature {signature:?} got {status}");
    }
    ensure!(h.mock.read(|r| r.activities.is_empty()));
    ensure!(h.calls("claude").is_empty());
    Ok(())
}

/// A stored token Linear rejects is refreshed through `LINEAR_API_URL` before the workspace
/// is pinned, and the new pair is saved.
async fn linear_token_is_refreshed_at_startup(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        token: mock::EXPIRED_TOKEN,
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    let grants = h.mock.read(|r| r.token_grants.clone());
    ensure!(grants.len() == 1, "{grants:?}");
    ensure!(grants[0]["grant_type"] == "refresh_token" && grants[0]["refresh_token"] == "refresh");
    let state: Value =
        serde_json::from_slice(&std::fs::read(h.home.join(".mothership/state.json"))?)?;
    ensure!(
        state["linear"]["access_token"] == mock::FRESH_TOKEN,
        "{state}"
    );
    Ok(())
}

/// With two repositories, `[repo=…]` in the issue picks one; without it the session asks, and
/// the reply both picks one and runs the first prompt that was kept.
async fn linear_routes_between_two_repositories(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        repos: vec![("app", "o/app"), ("lib", "o/lib")],
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;

    let tagged = issue(3, "Speed it up [repo=lib]");
    h.mock.add_issue(&tagged);
    ensure!(h.linear(created("s3", &tagged, "context of SH-3")).await? == 200);
    h.activity("s3", "response", 1).await?;
    let thought = h.activity("s3", "thought", 1).await?;
    ensure!(
        body(&thought) == "Working in `lib` (`[repo=lib]` in the issue).",
        "{thought}"
    );
    let call = h.prompt_with("context of SH-3").await?;
    let cwd = Path::new(call["cwd"].as_str().context("cwd")?);
    ensure!(main_clone(cwd)? == h.home.join("src/lib").display().to_string());

    let untagged = issue(4, "No hint here.");
    h.mock.add_issue(&untagged);
    ensure!(
        h.linear(created("s4", &untagged, "context of SH-4"))
            .await?
            == 200
    );
    let question = h.activity("s4", "elicitation", 1).await?;
    ensure!(
        body(&question).starts_with("I can't tell which repository"),
        "{question}"
    );
    ensure!(
        h.prompts_with("context of SH-4").is_empty(),
        "no turn before routing"
    );

    ensure!(
        h.linear(prompted("s4", &untagged, "[repo=app] please"))
            .await?
            == 200
    );
    h.activity("s4", "response", 1).await?;
    let call = h.prompt_with("[repo=app] please").await?;
    let prompt = call["prompt"].as_str().unwrap_or_default();
    ensure!(
        prompt == "context of SH-4\n\n[repo=app] please",
        "{prompt:?}"
    );
    let cwd = Path::new(call["cwd"].as_str().context("cwd")?);
    ensure!(main_clone(cwd)? == h.home.join("src/app").display().to_string());
    Ok(())
}

/// A prompt that arrives mid-turn goes to the running agent instead of a new one.
async fn linear_prompt_joins_the_running_turn(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let (issue, pid, _) = held_session(&h, "s5", 5).await?;
    let more = format!("one more thing {RELEASE}");
    ensure!(h.linear(prompted("s5", &issue, &more)).await? == 200);

    let response = h.activity("s5", "response", 1).await?;
    ensure!(body(&response) == format!("Done: {more}"), "{response}");
    let noted = h.activities("s5");
    ensure!(
        noted
            .iter()
            .any(|a| body(a) == "Got it, adding that to the current work."),
        "{noted:?}"
    );
    let pids: Vec<_> = h.calls("claude").iter().map(|c| c["pid"].clone()).collect();
    ensure!(
        pids == [json!(pid), json!(pid)],
        "one agent took both: {pids:?}"
    );
    Ok(())
}

/// Linear's stop signal kills the running agent and the tool it runs, and says so.
async fn linear_stop_kills_the_agent(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let (issue, pid, tool) = held_session(&h, "s6", 6).await?;
    ensure!(h.linear(stop("s6", &issue)).await? == 200);
    let response = h.activity("s6", "response", 1).await?;
    ensure!(body(&response) == "Stopped.", "{response}");
    exited(pid).await?;
    exited(tool).await
}

fn pull_request(head_repo: &str) -> Value {
    json!({
        "number": 7,
        "html_url": "https://github.com/o/r/pull/7",
        "head": {"ref": "nate/sh-8", "repo": {"full_name": head_repo}},
    })
}

fn line_comment(id: u64, association: &str, text: &str, head_repo: &str) -> Value {
    json!({
        "action": "created",
        "repository": {"full_name": "o/r"},
        "pull_request": pull_request(head_repo),
        "comment": {
            "id": id, "user": {"login": "someone", "type": "User"},
            "author_association": association, "body": text,
            "path": "src/lib.rs", "line": 3, "html_url": format!("https://github.com/o/r/pull/7#r{id}"),
        },
    })
}

/// The repository owner's comments addressed to the agent on the session's pull request start
/// turns, once per delivery; outsiders, comments not mentioning the agent and fork pull requests
/// are ignored. `issue_comment` learns the head branch through `gh`.
async fn github_feedback_continues_the_session(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        github: true,
        env: vec![("FAKE_GH_HEAD_REF", "nate/sh-8".into())],
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    let issue = issue(8, "Ship it.");
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s8", &issue, "context of SH-8")).await? == 200);
    h.activity("s8", "response", 1).await?;

    let event = "pull_request_review_comment";
    let outsider = line_comment(1, "NONE", "@impala outsider says rm -rf", "o/r");
    ensure!(h.github(event, "d-1", &outsider).await? == 200);
    let fork = line_comment(2, "OWNER", "@impala fork comment", "mallory/r");
    ensure!(h.github(event, "d-2", &fork).await? == 200);
    let aside = line_comment(6, "OWNER", "aside to a reviewer", "o/r");
    ensure!(h.github(event, "d-6", &aside).await? == 200);
    let owner = line_comment(3, "OWNER", "@Impala: owner line comment", "o/r");
    ensure!(h.github(event, "d-3", &owner).await? == 200);
    ensure!(h.github(event, "d-3", &owner).await? == 200, "redelivery");
    ensure!(h.activity("s8", "response", 2).await?.is_object());

    let conversation = json!({
        "action": "created",
        "repository": {"full_name": "o/r"},
        "issue": {"number": 7, "html_url": "https://github.com/o/r/pull/7", "pull_request": {"url": "x"}},
        "comment": {
            "id": 4, "user": {"login": "someone", "type": "User"},
            "author_association": "OWNER", "body": "owner conversation comment @impala", "html_url": "c",
        },
    });
    ensure!(h.github("issue_comment", "d-4", &conversation).await? == 200);
    let call = h.prompt_with("owner conversation comment").await?;
    ensure!(
        call["prompt"]
            .as_str()
            .is_some_and(|p| p.starts_with("Comment from @someone on GitHub pull request")),
        "{call}"
    );
    let gh = h.calls("gh");
    ensure!(
        gh.len() == 1 && gh[0]["args"] == json!(["api", "repos/o/r/pulls/7"]),
        "{gh:?}"
    );
    h.activity("s8", "response", 3).await?;

    ensure!(
        h.prompts_with("owner line comment").len() == 1,
        "delivered once"
    );
    ensure!(h.prompts_with("outsider says").is_empty(), "outsider heard");
    ensure!(h.prompts_with("fork comment").is_empty(), "fork heard");
    ensure!(
        h.prompts_with("aside to a reviewer").is_empty(),
        "comment without a mention heard"
    );
    Ok(())
}

/// A conversation comment on a pull request whose head `gh` reports in a fork is ignored,
/// even from the owner.
async fn github_fork_conversation_comment_is_ignored(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        github: true,
        env: vec![
            ("FAKE_GH_HEAD_REF", "nate/sh-22".into()),
            ("FAKE_GH_HEAD_REPO", "mallory/r".into()),
        ],
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    let issue = issue(22, "Ship it.");
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s22", &issue, "context of SH-22")).await? == 200);
    h.activity("s22", "response", 1).await?;

    let conversation = json!({
        "action": "created",
        "repository": {"full_name": "o/r"},
        "issue": {"number": 7, "html_url": "https://github.com/o/r/pull/7", "pull_request": {"url": "x"}},
        "comment": {
            "id": 5, "user": {"login": "someone", "type": "User"},
            "author_association": "OWNER", "body": "@impala fork conversation comment", "html_url": "c",
        },
    });
    ensure!(h.github("issue_comment", "d-5", &conversation).await? == 200);
    h.logged("head is in mallory/r, ignored").await?;
    let gh = h.calls("gh");
    ensure!(
        gh.len() == 1 && gh[0]["args"] == json!(["api", "repos/o/r/pulls/7"]),
        "{gh:?}"
    );
    ensure!(
        h.prompts_with("fork conversation comment").is_empty(),
        "fork heard"
    );
    Ok(())
}

/// A mention is acknowledged with :eyes:, answered in its topic, then marked :check:. A wrong
/// token is refused.
async fn zulip_mention_gets_a_reply(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        zulip: true,
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    let mention = |token: &str| {
        json!({
            "token": token, "trigger": "mention", "data": "@**Bot** how are we doing?",
            "message": {
                "id": 501, "type": "stream", "stream_id": 7, "subject": "deploy",
                "display_recipient": "ops", "sender_full_name": "Ann",
                "sender_email": "ann@zulip.test", "content": "@**Bot** how are we doing?",
            },
        })
    };
    ensure!(h.zulip(&mention("wrong")).await? == 401);
    ensure!(h.zulip(&mention(ZULIP_TOKEN)).await? == 200);

    let reply = eventually("a Zulip reply", || {
        h.mock.read(|r| r.zulip_messages.first().cloned())
    })
    .await?;
    let expected = "Done: From Ann (ann@zulip.test):\nhow are we doing?";
    ensure!(
        reply["type"] == "stream"
            && reply["to"] == "7"
            && reply["topic"] == "deploy"
            && reply["content"] == expected,
        "{reply:?}"
    );
    let reactions = eventually("the reaction swap", || {
        let reactions = h.mock.read(|r| r.reactions.clone());
        (reactions.len() >= 3).then_some(reactions)
    })
    .await?;
    let want = [("add", "eyes"), ("remove", "eyes"), ("add", "check")]
        .map(|(how, emoji)| (how.to_string(), 501, emoji.to_string()));
    ensure!(reactions == want, "{reactions:?}");
    let call = h.prompt_with("how are we doing?").await?;
    ensure!(call["surface"] == "zulip");
    Ok(())
}

/// SIGTERM stops running turns, kills their agents and tools, and exits cleanly.
async fn sigterm_stops_running_agents(ctx: Ctx) -> Result<()> {
    let mut h = Harness::start(&ctx, Setup::default()).await?;
    let (_, pid, tool) = held_session(&h, "s9", 9).await?;
    let status = h.terminate().await?;
    ensure!(status.success(), "mothership exited with {status}");
    let response = h.activity("s9", "response", 1).await?;
    ensure!(body(&response) == "Stopped.", "{response}");
    exited(pid).await?;
    exited(tool).await
}
