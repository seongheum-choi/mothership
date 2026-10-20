//! The scenarios: each starts its own mothership and drives it through webhooks.

use crate::{
    fake::{
        BACKGROUND, BACKGROUND_DONE, BACKGROUND_HANG, CLAUDE_LIMIT, CODEX_RELEASE, CODEX_SCRIPT,
        HANG, HOLD, HOST_COMMAND, HOST_RUN, PROGRESS, PROGRESS_LINE, RELEASE,
    },
    harness::{Ctx, Harness, Setup, ZULIP_TOKEN, create_repo, eventually, exited, git},
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
    linear_session_links_the_pull_requests_it_mentions,
    linear_agent_is_launched_in_the_sandbox,
    linear_progress_from_bash_becomes_a_thought,
    host_run_answers_a_codex_request,
    host_run_dies_with_the_turn,
    host_run_runs_a_repository_command,
    linear_mention_leaves_the_issue_state_alone,
    linear_mode_from_labels_shapes_the_agent,
    linear_mode_conflict_or_broken_file_holds_the_turn,
    linear_closed_issue_cleans_up_its_worktree,
    linear_closed_issue_keeps_worktrees_still_in_use,
    linear_refuses_other_workspaces_and_bad_signatures,
    linear_token_is_refreshed_at_startup,
    linear_routes_between_two_repositories,
    linear_refused_reply_is_not_kept,
    linear_lost_worktree_is_cut_again_where_its_branch_is,
    linear_prompt_joins_the_running_turn,
    linear_stop_kills_the_agent,
    linear_agent_that_never_starts_is_stopped,
    linear_background_work_outlives_the_turn,
    linear_background_work_past_the_limit_is_given_up,
    github_feedback_continues_the_session,
    github_fork_conversation_comment_is_ignored,
    github_feedback_reaches_a_lost_worktree_session,
    zulip_mention_gets_a_reply,
    sigterm_stops_running_agents,
    reload_routes_to_a_repository_added_between_turns,
    reload_keeps_the_repositories_when_repos_json_breaks,
    reload_logs_a_restart_setting_change,
    model_and_effort_from_the_mention,
    model_and_effort_from_a_reply_apply_next_turn,
    unknown_effort_is_refused,
    instance_effort_applies_without_a_restart,
    zulip_effort_from_the_message,
    codex_runner_answers_with_thoughts_and_a_response,
    codex_runner_resumes_the_thread_with_a_prompt_from_mid_turn,
    claude_usage_limit_moves_turns_to_codex_until_it_resets,
    runner_directive_keeps_the_session_on_codex,
    runner_directive_keeps_the_session_on_claude_at_the_limit,
    repo_and_mode_models_apply_per_field,
    model_labels_beat_the_mode_and_bad_ones_are_ignored,
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
    ensure!(
        h.mock.read(|r| r.session_updates.is_empty()),
        "no pull request was mentioned, so no external URL update"
    );
    Ok(())
}

/// A pull request URL in a turn's response is added to the session's external URLs once,
/// without fragment or query; mentioning it again in a later turn adds nothing.
async fn linear_session_links_the_pull_requests_it_mentions(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let issue = issue(2, "Open a PR.");
    h.mock.add_issue(&issue);
    // The fake agent answers `Done: <prompt>`, so the prompt puts the URL in the response.
    let context = "Opened https://github.com/acme/app/pull/7#discussion_r1";
    ensure!(h.linear(created("s2", &issue, context)).await? == 200);
    h.activity("s2", "response", 1).await?;
    let update = eventually("agentSessionUpdate", || {
        h.mock.read(|r| r.session_updates.first().cloned())
    })
    .await?;
    let link = json!({"label": "acme/app#7", "url": "https://github.com/acme/app/pull/7"});
    ensure!(
        update == json!({"id": "s2", "input": {"addedExternalUrls": [link]}}),
        "{update}"
    );
    let recorded = eventually("the recorded pull request", || {
        let sessions = h.sessions().ok()?;
        (sessions["s2"]["pull_requests"] == json!([link["url"]])).then_some(())
    })
    .await;
    ensure!(recorded.is_ok(), "{:?}", h.sessions());

    let again = "Still https://github.com/acme/app/pull/7?tab=files";
    ensure!(h.linear(prompted("s2", &issue, again)).await? == 200);
    h.activity("s2", "response", 2).await?;
    let updates = h.mock.read(|r| r.session_updates.clone());
    ensure!(updates.len() == 1, "added once: {updates:?}");
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

/// The rule that keeps every file-editing tool out of a directory or a file.
fn deny_edit_dir(path: &Path) -> String {
    format!("Edit(/{}/**)", path.display())
}

fn deny_edit_file(path: &Path) -> String {
    format!("Edit(/{})", path.display())
}

fn thoughts(h: &Harness, sid: &str) -> Vec<String> {
    h.activities(sid)
        .iter()
        .filter(|a| a["type"] == "thought")
        .map(|a| body(a).to_string())
        .collect()
}

/// Without a mode, the agent bypasses permission prompts on the configured models, and its
/// settings deny reading everything in `HOME` but the worktree and the main clone, deny
/// editing everything in `MOTHERSHIP_HOME` but the worktree, and run Bash in Claude Code's
/// OS sandbox.
async fn linear_agent_is_launched_in_the_sandbox(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let issue = issue(10, "Fix it.");
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s10", &issue, "context of SH-10")).await? == 200);
    h.activity("s10", "response", 1).await?;

    let args = argv(&h.prompt_with("context of SH-10").await?);
    let state = h.home.join(".mothership");
    // `HOME` holds `.mothership` and `src/app`; the worktree is `.mothership/worktrees/SH-10`.
    // `host-runs/` holds runs' private directories, which no session may read or write.
    let mut deny = vec![
        deny_dir(&state.join("host-runs")),
        deny_dir(&state.join("mcp")),
        deny_file(&state.join("state.json")),
    ];
    deny.push(deny_edit_dir(&state.join("host-runs")));
    deny.push(deny_edit_dir(&state.join("mcp")));
    // `repos/` and `worktrees/` hold the empty `.env` that stops dotenv searches; it stays
    // readable, and like the rest of the state, not editable.
    deny.push(deny_edit_dir(&state.join("repos")));
    deny.push(deny_edit_file(&state.join("state.json")));
    deny.push(deny_edit_file(&state.join("worktrees/.env")));
    // The clone's `.git` stays editable for git's own writes, but for its config and hooks.
    let clone_git = h.home.join("src/app/.git");
    deny.push(deny_edit_file(&clone_git.join("config")));
    deny.push(deny_edit_dir(&clone_git.join("hooks")));
    let settings = json!({
        "permissions": {"deny": deny},
        "sandbox": {
            "enabled": true,
            "allowUnsandboxedCommands": false,
            "failIfUnavailable": true,
            "filesystem": {"allowWrite": [state.join("progress/s10")]},
            "network": {"allowLocalBinding": true, "allowMachLookup": ["com.apple.trustd.agent"]},
            "credentials": {"envVars": [{"name": "CLAUDE_CODE_OAUTH_TOKEN", "mode": "deny"}]},
        },
    });
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

/// A line a shell command appends to `MOTHERSHIP_PROGRESS_FILE` shows up as a thought, and
/// Bash's sandbox may write and read the session's progress directory: it gets no deny rule.
async fn linear_progress_from_bash_becomes_a_thought(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let issue = issue(13, "Report as you go.");
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s13", &issue, "first turn")).await? == 200);
    h.activity("s13", "response", 1).await?;
    // The second turn's launch sees the progress directory the first one created.
    let prompt = format!("report {PROGRESS}");
    ensure!(h.linear(prompted("s13", &issue, &prompt)).await? == 200);
    h.activity("s13", "response", 2).await?;

    // Progress is read on a timer and once more after the result, so it may trail the response.
    eventually("progress thought", || {
        thoughts(&h, "s13")
            .contains(&PROGRESS_LINE.to_string())
            .then_some(())
    })
    .await?;
    let args = argv(&h.prompt_with(&prompt).await?);
    let settings: Value = serde_json::from_str(flag(&args, "--settings")?)?;
    // Of the progress directory, a session may write only its own.
    let progress = h.home.join(".mothership/progress/s13");
    ensure!(
        settings["sandbox"]["filesystem"]["allowWrite"] == json!([progress]),
        "{settings}"
    );
    let deny = deny_rules(&args)?;
    ensure!(
        !deny.iter().any(|r| r.contains("/progress")),
        "progress directory denied: {deny:#?}"
    );
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
    let mut expected = vec![
        deny_dir(&state.join("host-runs")),
        deny_dir(&state.join("mcp")),
        deny_dir(&state.join("modes")),
        deny_file(&state.join("state.json")),
    ];
    expected.push(deny_edit_dir(&state.join("host-runs")));
    expected.push(deny_edit_dir(&state.join("mcp")));
    expected.push(deny_edit_dir(&state.join("modes")));
    expected.push(deny_edit_dir(&state.join("repos")));
    expected.push(deny_edit_file(&state.join("state.json")));
    expected.push(deny_edit_file(&state.join("worktrees/.env")));
    let clone_git = h.home.join("src/app/.git");
    expected.push(deny_edit_file(&clone_git.join("config")));
    expected.push(deny_edit_dir(&clone_git.join("hooks")));
    expected.extend(["Edit".to_string(), "Bash(git push:*)".to_string()]);
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

/// With two repositories, `[repo=…]` in the issue picks one; without it the session asks,
/// keeps replies that do not pick one, and the reply that does runs the first prompt with all
/// of them.
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
        h.linear(prompted("s4", &untagged, "It is the login page."))
            .await?
            == 200
    );
    h.activity("s4", "elicitation", 2).await?;
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
    let thought = h.activity("s4", "thought", 1).await?;
    ensure!(
        body(&thought) == "Working in `app` (`[repo=app]` in your message).",
        "{thought}"
    );
    let call = h.prompt_with("[repo=app] please").await?;
    let prompt = call["prompt"].as_str().unwrap_or_default();
    ensure!(
        prompt == "context of SH-4\n\nIt is the login page.\n\n[repo=app] please",
        "{prompt:?}"
    );
    let cwd = Path::new(call["cwd"].as_str().context("cwd")?);
    ensure!(main_clone(cwd)? == h.home.join("src/app").display().to_string());
    Ok(())
}

/// A reply whose directive is refused while the session waits for its repository is not kept,
/// so the corrected reply that starts the work reaches the agent once.
async fn linear_refused_reply_is_not_kept(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        repos: vec![("app", "o/app"), ("lib", "o/lib")],
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    let issue = issue(33, "No hint here.");
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s33", &issue, "context of SH-33")).await? == 200);
    h.activity("s33", "elicitation", 1).await?;

    let refused = "[effort=ultra] [repo=app] it is the login page";
    ensure!(h.linear(prompted("s33", &issue, refused)).await? == 200);
    let error = h.activity("s33", "error", 1).await?;
    ensure!(
        body(&error).starts_with("`[effort=ultra]` is not an effort level"),
        "{error}"
    );

    let corrected = "[effort=high] [repo=app] it is the login page";
    ensure!(h.linear(prompted("s33", &issue, corrected)).await? == 200);
    let call = h.prompt_with(corrected).await?;
    let prompt = call["prompt"].as_str().unwrap_or_default();
    ensure!(
        prompt == format!("context of SH-33\n\n{corrected}"),
        "{prompt:?}"
    );
    ensure!(flag(&argv(&call), "--effort")? == "high", "{call}");
    Ok(())
}

/// A session from before repository routing whose worktree is gone (here as closed-issue
/// cleanup leaves it: no workspace, the branch kept) goes to the repository that has its
/// branch, ahead of what the issue says, and its worktree is cut again there on that branch.
async fn linear_lost_worktree_is_cut_again_where_its_branch_is(ctx: Ctx) -> Result<()> {
    let issue = issue(40, "Speed it up [repo=app]");
    let setup = Setup {
        repos: vec![("app", "o/app"), ("lib", "o/lib")],
        state: Some(json!({"sessions": {"s40": {
            "issue_id": issue["id"], "identifier": issue["identifier"],
            "title": issue["title"], "url": issue["url"],
            "branch": "nate/sh-40",
        }}})),
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    h.mock.add_issue(&issue);
    let lib = h.home.join("src/lib");
    let work = git(
        &lib,
        &[
            "commit-tree",
            "-m",
            "work so far",
            "-p",
            "HEAD",
            "HEAD^{tree}",
        ],
    )?;
    git(&lib, &["update-ref", "refs/heads/nate/sh-40", work.trim()])?;

    ensure!(h.linear(prompted("s40", &issue, "carry on")).await? == 200);
    let thought = h.activity("s40", "thought", 1).await?;
    ensure!(
        body(&thought)
            == "Working in `lib` (the repository with this session's branch `nate/sh-40`).",
        "{thought}"
    );
    let call = h.prompt_with("carry on").await?;
    let cwd = Path::new(call["cwd"].as_str().context("cwd")?);
    ensure!(main_clone(cwd)? == lib.display().to_string());
    ensure!(git(cwd, &["branch", "--show-current"])?.trim() == "nate/sh-40");
    ensure!(git(cwd, &["log", "-1", "--format=%s"])?.trim() == "work so far");
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

/// An agent that announces no session within `AGENT_START_TIMEOUT` is killed with the tool it
/// runs, and the session gets an error that points at the README's macOS privacy section.
async fn linear_agent_that_never_starts_is_stopped(ctx: Ctx) -> Result<()> {
    let env = vec![("AGENT_START_TIMEOUT", "1".into()), (HANG, "1".into())];
    let h = Harness::start(
        &ctx,
        Setup {
            env,
            ..Setup::default()
        },
    )
    .await?;
    let issue = issue(23, "Hang.");
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s23", &issue, "context of SH-23")).await? == 200);

    let error = h.activity("s23", "error", 1).await?;
    ensure!(
        body(&error).starts_with("The agent did not start within 1s")
            && body(&error).contains("#agent-does-not-start-macos-privacy-prompts"),
        "{error}"
    );
    let call = h
        .calls("claude")
        .first()
        .cloned()
        .context("fake agent call")?;
    exited(call["pid"].as_u64().context("fake agent pid")?).await?;
    exited(call["tool_pid"].as_u64().context("fake tool pid")?).await
}

/// A turn that ends while its background task runs keeps the agent until the task finishes, and
/// the answer it wakes up to reaches the thread with nobody prompting it.
async fn linear_background_work_outlives_the_turn(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let issue = issue(62, "Build it in the background.");
    h.mock.add_issue(&issue);
    let context = format!("context of SH-62 {BACKGROUND}");
    ensure!(h.linear(created("s62", &issue, &context)).await? == 200);

    let first = h.activity("s62", "response", 1).await?;
    ensure!(body(&first) == "Started in the background.", "{first}");
    let second = h.activity("s62", "response", 2).await?;
    ensure!(body(&second) == BACKGROUND_DONE, "{second}");
    ensure!(
        thoughts(&h, "s62").contains(&waiting_note(1)),
        "{:?}",
        thoughts(&h, "s62")
    );
    let call = h.prompt_with(&context).await?;
    ensure!(h.calls("claude").len() == 1, "no relaunch was needed");
    exited(call["pid"].as_u64().context("fake agent pid")?).await
}

/// Background work still running after `AGENT_BACKGROUND_TIMEOUT` is given up on: the agent's
/// input closes so it can exit, and the thread is told.
async fn linear_background_work_past_the_limit_is_given_up(ctx: Ctx) -> Result<()> {
    let env = vec![("AGENT_BACKGROUND_TIMEOUT", "1".into())];
    let h = Harness::start(
        &ctx,
        Setup {
            env,
            ..Setup::default()
        },
    )
    .await?;
    let issue = issue(63, "Never finishes.");
    h.mock.add_issue(&issue);
    let context = format!("context of SH-63 {BACKGROUND_HANG}");
    ensure!(h.linear(created("s63", &issue, &context)).await? == 200);

    h.activity("s63", "response", 1).await?;
    eventually("give-up thought", || {
        thoughts(&h, "s63")
            .iter()
            .any(|t| t.starts_with("Background work is still running after 1s"))
            .then_some(())
    })
    .await?;
    let call = h.prompt_with(&context).await?;
    exited(call["pid"].as_u64().context("fake agent pid")?).await
}

fn waiting_note(tasks: usize) -> String {
    format!("Waiting for {tasks} background task(s) to finish before the agent exits.")
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

/// Feedback on the branch of a session from before repository routing whose worktree is gone
/// reaches it through the repository that has the branch, and its worktree is cut again there.
async fn github_feedback_reaches_a_lost_worktree_session(ctx: Ctx) -> Result<()> {
    let issue = issue(41, "Speed it up.");
    let setup = Setup {
        repos: vec![("app", "o/app"), ("lib", "o/lib")],
        github: true,
        state: Some(json!({"sessions": {"s41": {
            "issue_id": issue["id"], "identifier": issue["identifier"],
            "title": issue["title"], "url": issue["url"],
            "branch": "nate/sh-41", "workspace": "/nonexistent/worktrees/SH-41",
        }}})),
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    h.mock.add_issue(&issue);
    let lib = h.home.join("src/lib");
    git(&lib, &["branch", "nate/sh-41"])?;

    let comment = json!({
        "action": "created",
        "repository": {"full_name": "o/lib"},
        "pull_request": {
            "number": 9,
            "html_url": "https://github.com/o/lib/pull/9",
            "head": {"ref": "nate/sh-41", "repo": {"full_name": "o/lib"}},
        },
        "comment": {
            "id": 41, "user": {"login": "someone", "type": "User"},
            "author_association": "OWNER", "body": "@impala tidy this up",
            "path": "src/lib.rs", "line": 3, "html_url": "https://github.com/o/lib/pull/9#r41",
        },
    });
    ensure!(
        h.github("pull_request_review_comment", "d-41", &comment)
            .await?
            == 200
    );
    let call = h.prompt_with("tidy this up").await?;
    let cwd = Path::new(call["cwd"].as_str().context("cwd")?);
    ensure!(main_clone(cwd)? == lib.display().to_string());
    ensure!(git(cwd, &["branch", "--show-current"])?.trim() == "nate/sh-41");
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

/// Writes `repos.json` with the git repositories under `<home>/src` named in `names`.
fn write_repos(h: &Harness, names: &[&str]) -> Result<()> {
    let repos: Vec<Value> = names
        .iter()
        .map(|name| json!({ "name": name, "path": h.home.join("src").join(name) }))
        .collect();
    std::fs::write(
        h.home.join(".mothership/repos.json"),
        Value::Array(repos).to_string(),
    )?;
    Ok(())
}

/// A repository added to `repos.json` while running takes the next session, and GitHub feedback
/// starts hearing its origin.
async fn reload_routes_to_a_repository_added_between_turns(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        repos: vec![("app", "o/app"), ("lib", "o/lib")],
        github: true,
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    create_repo(&h.home.join("src/web"), "o/web")?;
    write_repos(&h, &["app", "lib", "web"])?;

    let tagged = issue(5, "Restyle it [repo=web]");
    h.mock.add_issue(&tagged);
    ensure!(h.linear(created("s5", &tagged, "context of SH-5")).await? == 200);
    h.activity("s5", "response", 1).await?;
    let thought = h.activity("s5", "thought", 1).await?;
    ensure!(
        body(&thought) == "Working in `web` (`[repo=web]` in the issue).",
        "{thought}"
    );
    let call = h.prompt_with("context of SH-5").await?;
    let cwd = Path::new(call["cwd"].as_str().context("cwd")?);
    ensure!(main_clone(cwd)? == h.home.join("src/web").display().to_string());
    h.logged("settings: repositories reloaded: app, lib, web")
        .await?;
    h.logged("github feedback accepted for o/app, o/lib, o/web")
        .await
}

/// A `repos.json` that no longer loads is reported, and sessions keep the repositories they had.
async fn reload_keeps_the_repositories_when_repos_json_breaks(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        repos: vec![("app", "o/app"), ("lib", "o/lib")],
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    std::fs::write(h.home.join(".mothership/repos.json"), "[{\"name\": ")?;

    let tagged = issue(6, "Speed it up [repo=lib]");
    h.mock.add_issue(&tagged);
    ensure!(h.linear(created("s6", &tagged, "context of SH-6")).await? == 200);
    h.activity("s6", "response", 1).await?;
    let call = h.prompt_with("context of SH-6").await?;
    let cwd = Path::new(call["cwd"].as_str().context("cwd")?);
    ensure!(main_clone(cwd)? == h.home.join("src/lib").display().to_string());
    h.logged("; keeping the current repositories").await
}

/// A setting only read at startup, changed in `.env`, is named in the log, not applied, and
/// its value stays out of the log.
async fn reload_logs_a_restart_setting_change(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    std::fs::write(
        h.home.join(".mothership/.env"),
        "GITHUB_WEBHOOK_SECRET=rotated-secret\n",
    )?;

    let issue = issue(7, "Fix the thing.");
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s7", &issue, "context of SH-7")).await? == 200);
    h.activity("s7", "response", 1).await?;
    h.logged("settings: GITHUB_WEBHOOK_SECRET changed in")
        .await?;
    h.logged("restart mothership to apply it").await?;
    let log = std::fs::read_to_string(h.dir.join("mothership.log"))?;
    ensure!(!log.contains("rotated-secret"), "the value was logged");
    ensure!(
        h.post("/github-webhook", &[], b"{}".to_vec()).await? == 404,
        "GitHub feedback stays off until a restart"
    );
    Ok(())
}

/// `[model=…]` and `[effort=…]` in the mention that starts a session shape its first turn, and
/// its first thought says so.
async fn model_and_effort_from_the_mention(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let issue = issue(30, "Fix it.");
    h.mock.add_issue(&issue);
    let comment = "@mothership [model=sonnet] [effort=high] take a look";
    ensure!(
        h.linear(mentioned("s30", &issue, comment, "context of SH-30"))
            .await?
            == 200
    );
    h.activity("s30", "response", 1).await?;
    let args = argv(&h.prompt_with("context of SH-30").await?);
    ensure!(flag(&args, "--model")? == "sonnet", "{args:#?}");
    ensure!(flag(&args, "--fallback-model")? == "sonnet", "{args:#?}");
    ensure!(flag(&args, "--effort")? == "high", "{args:#?}");
    let thoughts = thoughts(&h, "s30");
    // The turn's first thought, after the routing one and the ephemeral "Working on it…".
    let turn = thoughts
        .iter()
        .skip_while(|t| *t != "Working on it…")
        .nth(1);
    ensure!(
        turn.map(String::as_str) == Some("Model: sonnet, effort: high (from your message)"),
        "{thoughts:?}"
    );
    Ok(())
}

/// A reply's `[effort=…]` leaves the running turn alone and applies from the next one, which
/// still resumes the conversation.
async fn model_and_effort_from_a_reply_apply_next_turn(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let (issue, pid, _) = held_session(&h, "s31", 31).await?;
    let more = format!("[effort=max] then go deeper {RELEASE}");
    ensure!(h.linear(prompted("s31", &issue, &more)).await? == 200);
    h.activity("s31", "response", 1).await?;
    let held = h.prompt_with("then go deeper").await?;
    ensure!(held["pid"] == pid, "joined the running turn: {held}");
    ensure!(!argv(&held).contains(&"--effort".to_string()), "{held}");
    ensure!(thoughts(&h, "s31").iter().all(|t| !t.starts_with("Model:")));

    ensure!(h.linear(prompted("s31", &issue, "and the tests")).await? == 200);
    h.activity("s31", "response", 2).await?;
    let next = h.prompt_with("and the tests").await?;
    let args = argv(&next);
    ensure!(flag(&args, "--effort")? == "max", "{args:#?}");
    ensure!(flag(&args, "--model")? == "opus", "{args:#?}");
    ensure!(next["resume"] == format!("fake-{pid}"), "{next}");
    let thoughts = thoughts(&h, "s31");
    ensure!(
        thoughts
            .contains(&"Model: opus (instance default), effort: max (from your message)".into()),
        "{thoughts:?}"
    );
    Ok(())
}

/// An effort level Claude Code does not know is an error, and no turn runs.
async fn unknown_effort_is_refused(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let issue = issue(32, "Fix it.");
    h.mock.add_issue(&issue);
    let comment = "@mothership [effort=ultra] fix it";
    ensure!(
        h.linear(mentioned("s32", &issue, comment, "context of SH-32"))
            .await?
            == 200
    );
    let error = h.activity("s32", "error", 1).await?;
    ensure!(
        body(&error).starts_with("`[effort=ultra]` is not an effort level"),
        "{error}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    ensure!(h.calls("claude").is_empty(), "{:?}", h.calls("claude"));
    Ok(())
}

/// `CLAUDE_EFFORT` changed in `.env` between turns applies to the next turn without a restart,
/// which says so; it is not reported as needing one.
async fn instance_effort_applies_without_a_restart(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let issue = issue(33, "Fix it.");
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s33", &issue, "context of SH-33")).await? == 200);
    h.activity("s33", "response", 1).await?;
    let first = argv(&h.prompt_with("context of SH-33").await?);
    ensure!(!first.contains(&"--effort".to_string()), "{first:#?}");

    std::fs::write(h.home.join(".mothership/.env"), "CLAUDE_EFFORT=high\n")?;
    ensure!(h.linear(prompted("s33", &issue, "once more")).await? == 200);
    h.activity("s33", "response", 2).await?;
    let args = argv(&h.prompt_with("once more").await?);
    ensure!(flag(&args, "--effort")? == "high", "{args:#?}");
    let thoughts = thoughts(&h, "s33");
    ensure!(
        thoughts.contains(&"Model: opus, effort: high (instance default)".into()),
        "{thoughts:?}"
    );
    let log = std::fs::read_to_string(h.dir.join("mothership.log"))?;
    ensure!(!log.contains("restart mothership"), "{log}");
    Ok(())
}

/// Zulip messages take the same directives.
async fn zulip_effort_from_the_message(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        zulip: true,
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    let mention = |id: u64, text: &str| {
        json!({
            "token": ZULIP_TOKEN, "trigger": "private_message", "data": text,
            "message": {
                "id": id, "type": "private", "display_recipient": [{"id": 3}], "sender_id": 3,
                "sender_full_name": "Ann", "sender_email": "ann@zulip.test", "content": text,
            },
        })
    };
    ensure!(h.zulip(&mention(601, "[effort=low] quick one")).await? == 200);
    let args = argv(&h.prompt_with("quick one").await?);
    ensure!(flag(&args, "--effort")? == "low", "{args:#?}");
    ensure!(h.zulip(&mention(602, "[effort=huge] another")).await? == 200);
    eventually("the refusal", || {
        h.mock.read(|r| {
            r.zulip_messages
                .iter()
                .any(|m| {
                    m.get("content")
                        .is_some_and(|c| c.contains("[effort=huge]"))
                })
                .then_some(())
        })
    })
    .await?;
    ensure!(h.prompts_with("another").is_empty());
    Ok(())
}

/// What the fake codex plays back: a reasoning summary, a shell command and the answer.
const CODEX_TURN: &str = r#"{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"Looking into it."}}
{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":"","exit_code":null,"status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":"","exit_code":0,"status":"completed"}}
{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"Done: {prompt}"}}
{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":2}}
"#;

/// An instance whose `.env` says `AGENT_RUNNER=codex`, with the fake codex's script in place.
async fn codex_harness(ctx: &Ctx) -> Result<Harness> {
    let h = Harness::start(ctx, Setup::default()).await?;
    std::fs::write(h.dir.join(CODEX_SCRIPT), CODEX_TURN)?;
    std::fs::write(h.home.join(".mothership/.env"), "AGENT_RUNNER=codex\n")?;
    Ok(h)
}

/// With `AGENT_RUNNER=codex` in `.env`, a turn runs `codex exec --json` in the worktree with the
/// prompt on stdin; its reasoning becomes a thought, its command an action and its last message
/// the response. `/status` names the runner.
async fn codex_runner_answers_with_thoughts_and_a_response(ctx: Ctx) -> Result<()> {
    let h = codex_harness(&ctx).await?;
    ensure!(h.status().await?["runner"] == "codex");
    let issue = issue(40, "Fix it.");
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s40", &issue, "context of SH-40")).await? == 200);

    let response = h.activity("s40", "response", 1).await?;
    ensure!(body(&response) == "Done: context of SH-40", "{response}");
    let thoughts = thoughts(&h, "s40");
    ensure!(
        thoughts.contains(&"Looking into it.".into()),
        "{thoughts:?}"
    );
    ensure!(
        h.activities("s40")
            .iter()
            .any(|a| a["type"] == "action" && a["action"] == "Bash" && a["parameter"] == "ls"),
        "command shown as an action"
    );
    let call = h.prompt_with("context of SH-40").await?;
    ensure!(
        call["tool"] == "codex" && call["resume"].is_null(),
        "{call}"
    );
    ensure!(h.calls("claude").is_empty(), "{:?}", h.calls("claude"));
    let args = argv(&call);
    let cwd = h.home.join(".mothership/worktrees/SH-40");
    ensure!(
        args[..2] == ["exec", "--json"] && args.last().is_some_and(|a| a == "-"),
        "{args:#?}"
    );
    ensure!(
        flag(&args, "--cd")? == cwd.display().to_string(),
        "{args:#?}"
    );
    ensure!(flag(&args, "--sandbox")? == "workspace-write", "{args:#?}");
    ensure!(
        args.iter()
            .any(|a| a.starts_with("developer_instructions=") && a.contains("SH-40")),
        "{args:#?}"
    );
    ensure!(
        !args.contains(&"-m".to_string()),
        "the Claude default model is not passed to Codex: {args:#?}"
    );
    let sessions = h.sessions()?;
    ensure!(
        sessions["s40"]["agent_sessions"] == json!({"codex": format!("thread-{}", call["pid"])}),
        "{sessions}"
    );
    Ok(())
}

/// `codex exec` takes no input after its prompt, so a reply that arrives mid-turn waits for the
/// turn to end and then starts the next one, which resumes the first turn's thread.
async fn codex_runner_resumes_the_thread_with_a_prompt_from_mid_turn(ctx: Ctx) -> Result<()> {
    let h = codex_harness(&ctx).await?;
    let issue = issue(41, "Hold on.");
    h.mock.add_issue(&issue);
    let context = format!("context of SH-41 {HOLD}");
    ensure!(h.linear(created("s41", &issue, &context)).await? == 200);
    let first = h.prompt_with(&context).await?;
    ensure!(h.linear(prompted("s41", &issue, "and the tests")).await? == 200);
    // The reply is queued shortly after it is logged.
    h.logged(r#"[s41] linear "prompted""#).await?;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    ensure!(
        h.prompts_with("and the tests").is_empty(),
        "nothing reaches the running codex"
    );
    std::fs::write(h.dir.join(CODEX_RELEASE), "")?;

    let response = h.activity("s41", "response", 1).await?;
    ensure!(body(&response) == format!("Done: {context}"), "{response}");
    let second = h.prompt_with("and the tests").await?;
    let thread = format!("thread-{}", first["pid"]);
    ensure!(second["resume"] == thread, "{second}");
    ensure!(flag(&argv(&second), "resume")? == thread);
    ensure!(second["pid"] != first["pid"], "a new process");
    let response = h.activity("s41", "response", 2).await?;
    ensure!(body(&response) == "Done: and the tests", "{response}");
    ensure!(
        thoughts(&h, "s41")
            .iter()
            .all(|t| t != "Got it, adding that to the current work."),
        "not noted as joining the running turn"
    );
    Ok(())
}

/// A Claude turn that ends on the usage limit runs again, same prompt, on Codex, and says so
/// first. Until the limit resets, the next turn runs on Codex and resumes its thread; after it,
/// the session is back on Claude Code and resumes the Claude session.
async fn claude_usage_limit_moves_turns_to_codex_until_it_resets(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    std::fs::write(h.dir.join(CODEX_SCRIPT), CODEX_TURN)?;
    std::fs::write(h.dir.join(CLAUDE_LIMIT), "6")?;
    // The hold below is Claude's; Codex must not honour it.
    std::fs::write(h.dir.join(CODEX_RELEASE), "")?;
    let issue = issue(50, "Fix it.");
    h.mock.add_issue(&issue);
    let context = format!("context of SH-50 {HOLD}");
    ensure!(h.linear(created("s50", &issue, &context)).await? == 200);
    h.prompt_with(&context).await?;
    // A reply that joined the turn before the limit hit goes to Codex with it.
    let more = format!("and the docs {RELEASE}");
    ensure!(h.linear(prompted("s50", &issue, &more)).await? == 200);

    let response = h.activity("s50", "response", 1).await?;
    let prompt = format!("{context}\n\n{more}");
    ensure!(body(&response) == format!("Done: {prompt}"), "{response}");
    let tried = h.calls("claude");
    ensure!(
        tried.len() == 2 && tried[0]["pid"] == tried[1]["pid"],
        "{tried:?}"
    );
    let codex = h.calls("codex");
    ensure!(
        codex.len() == 1 && codex[0]["prompt"] == prompt && codex[0]["resume"].is_null(),
        "both prompts on a new thread: {codex:?}"
    );
    let switched = "Claude usage limit reached → switching to Codex until ";
    ensure!(
        thoughts(&h, "s50").iter().any(|t| t.starts_with(switched)),
        "{:?}",
        thoughts(&h, "s50")
    );
    let resets_at = h.status().await?["claude_limited_until"].as_u64();
    let resets_at = resets_at.context("the limit holds after the switch")?;

    ensure!(h.linear(prompted("s50", &issue, "and the tests")).await? == 200);
    h.activity("s50", "response", 2).await?;
    let next = h.prompt_with("and the tests").await?;
    let thread = format!("thread-{}", codex[0]["pid"]);
    ensure!(
        next["tool"] == "codex" && next["resume"] == thread,
        "{next}"
    );
    ensure!(
        thoughts(&h, "s50").iter().any(
            |t| t.starts_with("Claude usage limit until ") && t.ends_with("→ running on Codex")
        ),
        "{:?}",
        thoughts(&h, "s50")
    );

    std::fs::remove_file(h.dir.join(CLAUDE_LIMIT))?;
    eventually("the limit to reset", || {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        now.is_ok_and(|n| n.as_secs() >= resets_at).then_some(())
    })
    .await?;
    ensure!(h.linear(prompted("s50", &issue, "back on claude")).await? == 200);
    h.activity("s50", "response", 3).await?;
    let back = h.prompt_with("back on claude").await?;
    ensure!(
        back["tool"] == "claude" && back["resume"] == format!("fake-{}", tried[0]["pid"]),
        "{back}"
    );
    ensure!(h.status().await?["claude_limited_until"].is_null());
    Ok(())
}

/// `[runner=codex]` on an instance that runs Claude Code moves the session to Codex, its first
/// thought says so, and the next turn stays there and resumes the thread.
async fn runner_directive_keeps_the_session_on_codex(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    std::fs::write(h.dir.join(CODEX_SCRIPT), CODEX_TURN)?;
    let issue = issue(60, "Fix it.");
    h.mock.add_issue(&issue);
    let comment = "@mothership [runner=codex] take a look";
    ensure!(
        h.linear(mentioned("s60", &issue, comment, "context of SH-60"))
            .await?
            == 200
    );
    h.activity("s60", "response", 1).await?;
    let first = h.prompt_with("context of SH-60").await?;
    ensure!(first["tool"] == "codex", "{first}");
    let said = thoughts(&h, "s60");
    ensure!(
        said.iter()
            .any(|t| t.starts_with("Runner: codex (from your message), model: opus")),
        "{said:?}"
    );

    ensure!(h.linear(prompted("s60", &issue, "and the tests")).await? == 200);
    h.activity("s60", "response", 2).await?;
    let next = h.prompt_with("and the tests").await?;
    let thread = format!("thread-{}", first["pid"]);
    ensure!(
        next["tool"] == "codex" && next["resume"] == thread,
        "{next}"
    );
    ensure!(h.calls("claude").is_empty(), "{:?}", h.calls("claude"));
    ensure!(
        h.status().await?["runner"] == "claude",
        "the instance default"
    );
    Ok(())
}

/// A session `[runner=claude]` pins that hits Claude Code's usage limit fails with the reason
/// instead of moving to Codex, and its next turn tries Claude Code again while the limit lasts.
async fn runner_directive_keeps_the_session_on_claude_at_the_limit(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    std::fs::write(h.dir.join(CODEX_SCRIPT), CODEX_TURN)?;
    std::fs::write(h.dir.join(CLAUDE_LIMIT), "600")?;
    let issue = issue(61, "Fix it.");
    h.mock.add_issue(&issue);
    let comment = "@mothership [runner=claude] take a look";
    ensure!(
        h.linear(mentioned("s61", &issue, comment, "context of SH-61"))
            .await?
            == 200
    );
    let error = h.activity("s61", "error", 1).await?;
    ensure!(
        body(&error).starts_with("You've hit your session limit")
            && body(&error).contains("pinned to Claude Code with `[runner=claude]`"),
        "{error}"
    );
    ensure!(h.status().await?["claude_limited_until"].is_u64());

    ensure!(h.linear(prompted("s61", &issue, "try again")).await? == 200);
    h.activity("s61", "error", 2).await?;
    let again = h.prompt_with("try again").await?;
    ensure!(again["tool"] == "claude", "{again}");
    ensure!(h.calls("codex").is_empty(), "{:?}", h.calls("codex"));
    ensure!(
        thoughts(&h, "s61")
            .iter()
            .all(|t| !t.contains("switching to Codex") && !t.contains("running on Codex")),
        "{:?}",
        thoughts(&h, "s61")
    );
    Ok(())
}

/// A repository's `model`, `fallback_model` and `effort` in `repos.json` apply to its sessions,
/// and a mode's `model` beats them field by field.
async fn repo_and_mode_models_apply_per_field(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    std::fs::write(
        h.home.join(".mothership/repos.json"),
        json!([{
            "name": "app",
            "path": h.home.join("src").join("app"),
            "model": "haiku",
            "fallback_model": "haiku",
            "effort": "low",
        }])
        .to_string(),
    )?;
    let issue = issue(41, "Tidy up.");
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s41", &issue, "context of SH-41")).await? == 200);
    h.activity("s41", "response", 1).await?;
    let args = argv(&h.prompt_with("context of SH-41").await?);
    ensure!(flag(&args, "--model")? == "haiku", "{args:#?}");
    ensure!(flag(&args, "--fallback-model")? == "haiku", "{args:#?}");
    ensure!(flag(&args, "--effort")? == "low", "{args:#?}");
    let said = thoughts(&h, "s41");
    ensure!(
        said.contains(&"Model: haiku, fallback: haiku, effort: low (repo `app`)".into()),
        "{said:?}"
    );

    let modes = h.home.join(".mothership/modes");
    std::fs::create_dir_all(&modes)?;
    std::fs::write(
        modes.join("research.md"),
        "---\nlabels: [Research]\nmodel: sonnet\n---\nAnswer; change nothing.\n",
    )?;
    let issue = labelled(42, "Why is it slow?", &["research"]);
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s42", &issue, "context of SH-42")).await? == 200);
    h.activity("s42", "response", 1).await?;
    let args = argv(&h.prompt_with("context of SH-42").await?);
    ensure!(flag(&args, "--model")? == "sonnet", "{args:#?}");
    ensure!(flag(&args, "--fallback-model")? == "haiku", "{args:#?}");
    ensure!(flag(&args, "--effort")? == "low", "{args:#?}");
    let said = thoughts(&h, "s42");
    ensure!(
        said.contains(
            &"Model: sonnet (mode `research`), fallback: haiku (repo `app`), \
              effort: low (repo `app`)"
                .into()
        ),
        "{said:?}"
    );
    Ok(())
}

/// An issue's `model:`/`effort:` label beats its mode, and a label that names no level is
/// ignored with the field falling through to the mode.
async fn model_labels_beat_the_mode_and_bad_ones_are_ignored(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let modes = h.home.join(".mothership/modes");
    std::fs::create_dir_all(&modes)?;
    std::fs::write(
        modes.join("research.md"),
        "---\nlabels: [Research]\nmodel: haiku\neffort: low\n---\nAnswer; change nothing.\n",
    )?;

    let issue = labelled(51, "Why?", &["research", "Model:sonnet", "effort:max"]);
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s51", &issue, "context of SH-51")).await? == 200);
    h.activity("s51", "response", 1).await?;
    let args = argv(&h.prompt_with("context of SH-51").await?);
    ensure!(flag(&args, "--model")? == "sonnet", "{args:#?}");
    ensure!(flag(&args, "--effort")? == "max", "{args:#?}");
    let said = thoughts(&h, "s51");
    ensure!(
        said.contains(
            &"Model: sonnet (label `Model:sonnet`), effort: max (label `effort:max`)".into()
        ),
        "{said:?}"
    );

    let issue = labelled(52, "Why else?", &["research", "effort:ultra"]);
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s52", &issue, "context of SH-52")).await? == 200);
    h.activity("s52", "response", 1).await?;
    let args = argv(&h.prompt_with("context of SH-52").await?);
    ensure!(flag(&args, "--model")? == "haiku", "{args:#?}");
    ensure!(flag(&args, "--effort")? == "low", "{args:#?}");
    let log = std::fs::read_to_string(h.dir.join("mothership.log"))?;
    ensure!(
        log.contains("label `effort:ultra` ignored: `ultra` is not an effort level"),
        "{log}"
    );
    Ok(())
}

/// A Codex run the agent asks for in `MOTHERSHIP_RUNS` runs under `HOST_SANDBOX` with a profile
/// made from the session's rules, Codex's own sandbox off, in the worktree; the answer lands
/// next to the request, and its start and end show up as thoughts.
async fn host_run_answers_a_codex_request(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let issue = issue(30, "Review it.");
    h.mock.add_issue(&issue);
    let prompt = format!("check {HOST_RUN}");
    ensure!(h.linear(created("s30", &issue, &prompt)).await? == 200);
    let response = h.activity("s30", "response", 1).await?;
    let worktree = h.home.join(".mothership/worktrees/SH-30");
    let answer: Value = body(&response)
        .strip_prefix("Host: ")
        .and_then(|a| serde_json::from_str(a).ok())
        .with_context(|| format!("{response}"))?;
    ensure!(
        answer == json!({"result": {"findings": [], "cwd": worktree}}),
        "{answer}"
    );

    let sandbox = h.calls("sandbox-exec");
    ensure!(sandbox.len() == 1, "{sandbox:#?}");
    let profile = sandbox[0]["profile"].as_str().unwrap_or_default();
    let subpath = |p: &Path| format!("(subpath \"{}\")", p.display());
    ensure!(
        profile.contains("(deny file-read* ")
            && profile.contains(&format!("(allow file-write* {}", subpath(&worktree)))
            && profile.contains(&subpath(&h.home.join(".codex/sessions")))
            && !profile.contains(&subpath(&h.home.join(".codex")))
            && profile.contains("(deny signal)")
            && profile.contains(&subpath(&h.home.join(".mothership/progress/s30"))),
        "{profile}"
    );
    let codex = h.calls("codex");
    ensure!(codex.len() == 1, "{codex:#?}");
    let args: Vec<String> = serde_json::from_value(codex[0]["args"].clone())?;
    for expected in [
        "--sandbox",
        "danger-full-access",
        "--output-schema",
        "model_reasoning_effort=\"high\"",
        worktree.to_str().unwrap_or_default(),
    ] {
        ensure!(
            args.iter().any(|a| a == expected),
            "{expected} not in {args:#?}"
        );
    }
    ensure!(
        codex[0]["prompt"]
            .as_str()
            .is_some_and(|p| p.contains(&prompt)),
        "{codex:#?}"
    );
    eventually("run thoughts", || {
        let t = thoughts(&h, "s30");
        (t.contains(&"Codex run r1 started".to_string())
            && t.contains(&"Codex run r1 finished".to_string()))
        .then_some(())
    })
    .await
}

/// Stopping the session kills a Codex run it asked for along with the agent.
async fn host_run_dies_with_the_turn(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let issue = issue(31, "Review it slowly.");
    h.mock.add_issue(&issue);
    let prompt = format!("check {HOST_RUN} {HOLD} {RELEASE}");
    ensure!(h.linear(created("s31", &issue, &prompt)).await? == 200);
    let codex = eventually("codex run", || h.calls("codex").into_iter().next()).await?;
    ensure!(h.linear(stop("s31", &issue)).await? == 200);
    let response = h.activity("s31", "response", 1).await?;
    ensure!(body(&response) == "Stopped.", "{response}");
    exited(codex["pid"].as_u64().unwrap_or_default()).await
}

/// A command the repository names in `repos.json` runs in the worktree when the session asks,
/// under the session's profile, and the session's system prompt lists it.
async fn host_run_runs_a_repository_command(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    std::fs::write(
        h.home.join(".mothership/repos.json"),
        json!([{
            "name": "app",
            "path": h.home.join("src/app"),
            "commands": {"setup": "echo made > made.txt && echo set up && echo warned >&2"},
        }])
        .to_string(),
    )?;
    let issue = issue(32, "Set it up.");
    h.mock.add_issue(&issue);
    let prompt = format!("prepare {HOST_COMMAND}");
    ensure!(h.linear(created("s32", &issue, &prompt)).await? == 200);
    let response = h.activity("s32", "response", 1).await?;
    let answer: Value = serde_json::from_str(
        body(&response)
            .strip_prefix("Host: ")
            .context("a host answer")?,
    )?;
    let result = answer["result"].as_str().context("a result")?;
    let log = result
        .strip_prefix("set up\nwarned\n\nWhole output: ")
        .context(format!("{result:?}"))?;
    ensure!(log.starts_with(&*h.home.to_string_lossy()), "{log}");
    ensure!(std::fs::read_to_string(log)? == "set up\nwarned\n");
    let worktree = h.home.join(".mothership/worktrees/SH-32");
    ensure!(std::fs::read_to_string(worktree.join("made.txt"))? == "made\n");
    let sandbox = h.calls("sandbox-exec");
    ensure!(
        sandbox.len() == 1 && sandbox[0]["program"] == "/bin/sh",
        "{sandbox:#?}"
    );
    let args = argv(&h.prompt_with(&prompt).await?);
    let system_prompt = flag(&args, "--append-system-prompt")?;
    ensure!(
        system_prompt
            .contains("- `setup`: `echo made > made.txt && echo set up && echo warned >&2`"),
        "{system_prompt}"
    );
    Ok(())
}
