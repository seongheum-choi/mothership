//! `mothership session status` and `reply`: the CLI asks the running instance through its
//! control socket, which reads Linear and posts replies in the session's thread.

use crate::{
    harness::{Ctx, Harness, Setup, Skipped},
    scenarios::{Scenario, body, created, issue, prompted},
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::process::Output;

macro_rules! scenarios {
    ($($name:ident),* $(,)?) => {
        &[$((stringify!($name), (|ctx| Box::pin($name(ctx))) as Scenario)),*]
    };
}

pub const ALL: &[(&str, Scenario)] = scenarios![
    cli_status_reads_every_activity_page,
    cli_reply_threads_under_the_root_comment,
    cli_refusals_write_nothing,
    cli_help_leaves_the_running_instance_alone,
];

/// `issue` with these agent sessions, as `issue { agentSessions }` returns them.
fn with_sessions(mut issue: Value, sessions: &[Value]) -> Value {
    issue["agentSessions"] = json!({ "nodes": sessions });
    issue
}

/// An agent session on Linear: its thread's root comment is `root`, when it has one.
fn agent_session(sid: &str, created_at: &str, root: Option<&str>) -> Value {
    let comment = root.map(|id| json!({ "id": id, "parent": null }));
    json!({ "id": sid, "status": "awaitingInput", "createdAt": created_at, "comment": comment })
}

/// A `state.json` whose sessions this instance ran on the issues `(sid, n)`.
fn known(sessions: &[(&str, u32)]) -> Value {
    let records: serde_json::Map<String, Value> = sessions
        .iter()
        .map(|(sid, n)| {
            let issue = issue(*n, "");
            let rec = json!({
                "issue_id": issue["id"], "identifier": issue["identifier"],
                "title": issue["title"], "url": issue["url"], "repo": "app",
            });
            ((*sid).to_string(), rec)
        })
        .collect();
    json!({ "sessions": records })
}

/// Starts the instance, or skips the scenario where this process may not make Unix sockets
/// (an agent's Seatbelt sandbox), since then mothership cannot either.
async fn start(ctx: &Ctx, setup: Setup) -> Result<Harness> {
    let probe = std::env::temp_dir().join(format!("mothership-sock-{}", std::process::id()));
    let _ = std::fs::remove_file(&probe);
    match std::os::unix::net::UnixListener::bind(&probe) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(Skipped("this sandbox does not allow Unix sockets").into());
        }
        _ => {
            let _ = std::fs::remove_file(&probe);
        }
    }
    Harness::start(ctx, setup).await
}

fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The newest report can sit past the first 250 activities; status reads every page and
/// says what each session runs, including one this instance does not know.
async fn cli_status_reads_every_activity_page(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        state: Some(known(&[("s1", 1)])),
        ..Setup::default()
    };
    let h = start(&ctx, setup).await?;
    let sessions = [
        agent_session("s0", "2026-11-24T08:00:00.000Z", Some("root-0")),
        agent_session("s1", "2026-11-24T09:00:00.000Z", Some("root-1")),
    ];
    h.mock.add_issue(&with_sessions(issue(1, ""), &sessions));
    let activities: Vec<Value> = (0..300)
        .map(|i| {
            let (kind, text) = match i {
                20 => (
                    "AgentActivityResponseContent",
                    "an early report".to_string(),
                ),
                280 => ("AgentActivityResponseContent", "the latest report".into()),
                _ => ("AgentActivityThoughtContent", format!("thought {i}")),
            };
            json!({
                "createdAt": format!("2026-11-24T10:{:02}:{:02}.000Z", i / 60, i % 60),
                "content": { "__typename": kind, "body": text },
            })
        })
        .collect();
    seed_activities(&h, "s1", activities);

    let status = h.cli(&["session", "status", "SH-1"], "").await?;
    ensure!(status.status.success(), "{}", err(&status));
    let text = out(&status);
    ensure!(text.contains("2 agent session(s)"), "{text}");
    ensure!(text.contains("the latest report"), "{text}");
    ensure!(!text.contains("an early report"), "{text}");
    ensure!(
        text.contains("last response at 2026-11-24T10:04:40.000Z"),
        "{text}"
    );
    ensure!(text.contains("linear: awaitingInput"), "{text}");
    ensure!(text.contains("mothership: idle\n"), "{text}");
    ensure!(text.contains("not a session of this instance"), "{text}");
    // Newest first.
    ensure!(text.find("session s1") < text.find("session s0"), "{text}");
    Ok(())
}

fn seed_activities(h: &Harness, sid: &str, activities: Vec<Value>) {
    h.mock.write(|r| {
        r.session_activities.insert(sid.to_string(), activities);
    });
}

/// A reply goes under the session's root comment, never top level, and starts the session's
/// next turn; the webhook Linear may send for that comment does not run it twice. A dry run
/// sends nothing.
async fn cli_reply_threads_under_the_root_comment(ctx: Ctx) -> Result<()> {
    let h = start(&ctx, Setup::default()).await?;
    let issue = issue(1, "Fix the thing.");
    h.mock.add_issue(&issue);
    ensure!(
        h.linear(created("s1", &issue, "<issue>SH-1</issue>"))
            .await?
            == 200
    );
    h.activity("s1", "response", 1).await?;
    let root = "root-1";
    let sessions = [agent_session("s1", "2026-11-24T09:00:00.000Z", Some(root))];
    h.mock.add_issue(&with_sessions(issue.clone(), &sessions));

    let dry = h
        .cli(&["session", "reply", "--dry-run", "SH-1"], "carry on")
        .await?;
    ensure!(dry.status.success(), "{}", err(&dry));
    ensure!(
        out(&dry).contains("session s1 on SH-1, under comment root-1"),
        "{}",
        out(&dry)
    );
    ensure!(
        h.mock.read(|r| r.comments.is_empty()),
        "a dry run posted a comment"
    );

    let reply = h.cli(&["session", "reply", "SH-1"], "carry on").await?;
    ensure!(reply.status.success(), "{}", err(&reply));
    ensure!(
        out(&reply).contains("(comment comment-1)"),
        "{}",
        out(&reply)
    );
    let comments = h.mock.read(|r| r.comments.clone());
    ensure!(
        comments == [json!({ "issueId": "issue-1", "parentId": root, "body": "carry on" })],
        "{comments:?}"
    );
    h.prompt_with("carry on").await?;
    let response = h.activity("s1", "response", 2).await?;
    ensure!(body(&response) == "Done: carry on", "{response}");

    let mut echo = prompted("s1", &issue, "carry on");
    echo["agentActivity"]["sourceCommentId"] = json!("comment-1");
    ensure!(h.linear(echo).await? == 200);
    h.logged("[s1] linear prompted: a reply from the CLI, already running")
        .await?;
    ensure!(h.prompts_with("carry on").len() == 1);
    Ok(())
}

/// An empty reply, an issue without a session of this instance and a session without a root
/// comment are refused with a non-zero exit, and nothing reaches Linear. With no instance, the
/// CLI says so.
async fn cli_refusals_write_nothing(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        state: Some(known(&[("s3", 3)])),
        ..Setup::default()
    };
    let mut h = start(&ctx, setup).await?;
    h.mock.add_issue(&with_sessions(issue(2, ""), &[]));
    let rootless = agent_session("s3", "2026-11-24T09:00:00.000Z", None);
    h.mock.add_issue(&with_sessions(issue(3, ""), &[rootless]));

    let refused = [
        (
            &["session", "reply", "SH-2"][..],
            "go",
            "SH-2 has no agent session of this instance",
        ),
        (
            &["session", "reply", "SH-3"],
            "go",
            "session s3 has no root comment",
        ),
        (
            &["session", "reply", "--dry-run", "SH-3"],
            "go",
            "no root comment",
        ),
        (&["session", "reply", "SH-3"], " \n", "the reply is empty"),
        (
            &["session", "status", "SH-9"],
            "",
            "Linear has no issue SH-9",
        ),
    ];
    for (args, stdin, why) in refused {
        let run = h.cli(args, stdin).await?;
        ensure!(!run.status.success(), "{args:?} succeeded: {}", out(&run));
        ensure!(err(&run).contains(why), "{args:?}: {}", err(&run));
    }
    ensure!(
        h.mock.read(|r| r.comments.is_empty()),
        "a refused reply posted"
    );
    ensure!(
        h.activities("s3").is_empty(),
        "a refused reply reached the session"
    );

    h.terminate().await?;
    let gone = h.cli(&["session", "status", "SH-3"], "").await?;
    ensure!(!gone.status.success());
    ensure!(
        err(&gone).contains("no running mothership answers"),
        "{}",
        err(&gone)
    );
    Ok(())
}

/// `--help` and unknown arguments print the usage and exit without starting a second server
/// or touching the running instance.
async fn cli_help_leaves_the_running_instance_alone(ctx: Ctx) -> Result<()> {
    let h = Harness::start(&ctx, Setup::default()).await?;
    let help = h.cli(&["--help"], "").await?;
    ensure!(help.status.success(), "{:?}: {}", help.status, err(&help));
    ensure!(out(&help).starts_with("Usage:"), "{}", out(&help));
    let unknown = h.cli(&["--port", "1"], "").await?;
    ensure!(unknown.status.code() == Some(2), "{:?}", unknown.status);
    ensure!(err(&unknown).starts_with("Usage:") && out(&unknown).is_empty());
    ensure!(
        h.status().await?["status"] == "idle",
        "the instance stopped answering"
    );
    let log = std::fs::read_to_string(h.dir.join("mothership.log"))?;
    ensure!(
        log.matches("listening on").count() == 1,
        "a second server started: {log}"
    );
    Ok(())
}
