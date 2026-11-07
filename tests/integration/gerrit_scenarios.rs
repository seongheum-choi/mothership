//! The Gerrit scenarios: pushing for review, answering on Gerrit, and review feedback reaching
//! the session.

use crate::{
    gerrit,
    harness::{Ctx, GERRIT_TOKEN, GERRIT_TRUSTED, Harness, Setup, Skipped, eventually},
    scenarios::{Scenario, argv, created, flag, issue},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio};
use tokio::io::AsyncWriteExt;

macro_rules! scenarios {
    ($($name:ident),* $(,)?) => {
        &[$((stringify!($name), (|ctx| Box::pin($name(ctx))) as Scenario)),*]
    };
}

pub const ALL: &[(&str, Scenario)] = scenarios![
    gerrit_sessions_push_for_review_and_reply,
    gerrit_review_continues_the_session_of_its_topic,
];

/// The repository's `origin`: a Gerrit project on the mock's host, over SSH as pilots clone it.
const ORIGIN: &str = "ssh://acme-bot@127.0.0.1:29418/team/app";

/// Runs the installed `gerrit-reply` with `args` and `body` on stdin, in the environment an
/// agent gets for this mock; returns its exit code. Async: the mock it calls runs on this
/// runtime, which may have a single worker.
async fn gerrit_reply(h: &Harness, args: &[&str], body: &str) -> Result<i32> {
    let url = &h.gerrit.as_ref().context("gerrit is on")?.url;
    let script = h.home.join(".mothership/gerrit-reply/gerrit-reply");
    let mut child = tokio::process::Command::new(script)
        .args(args)
        .env("GERRIT_URL", url)
        .env("GERRIT_USERNAME", gerrit::USERNAME)
        .env("GERRIT_HTTP_PASSWORD", gerrit::HTTP_PASSWORD)
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .context("piped stdin")?
        .write_all(body.as_bytes())
        .await?;
    Ok(child.wait().await?.code().unwrap_or(-1))
}

fn installed(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(tool).is_file()))
}

/// With `REVIEW_BACKEND=gerrit`, sessions are told to push to `refs/for/` with the issue key as
/// topic. `gerrit-reply` answers a comment in its thread, resolving it, or the patch set, both
/// tagged so mothership can tell them from feedback; an empty reply posts nothing.
async fn gerrit_sessions_push_for_review_and_reply(ctx: Ctx) -> Result<()> {
    if !installed("jq") || !installed("curl") {
        return Err(Skipped("gerrit-reply needs jq and curl").into());
    }
    let setup = Setup {
        gerrit: true,
        env: vec![("REVIEW_BACKEND", "gerrit".into())],
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    let issue = issue(30, "Ship it.");
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s30", &issue, "context of SH-30")).await? == 200);
    let call = h.prompt_with("context of SH-30").await?;
    let args = argv(&call);
    let system_prompt = flag(&args, "--append-system-prompt")?;
    ensure!(
        system_prompt.contains("HEAD:refs/for/<base>%topic=<issue key>"),
        "{system_prompt}"
    );
    ensure!(Path::new(&h.home.join(".mothership/gerrit-reply/gerrit-reply")).is_file());

    let quoted = "Done \"quoted\" and\n`code`.";
    ensure!(gerrit_reply(&h, &["12", "2", "c1", "src/a b.rs", "3"], quoted).await? == 0);
    ensure!(gerrit_reply(&h, &["12", "current"], "Thanks.").await? == 0);
    ensure!(
        gerrit_reply(&h, &["12", "current"], " \n").await? == 2,
        "empty"
    );
    let reviews = h
        .gerrit
        .as_ref()
        .context("gerrit is on")?
        .read(|r| r.reviews.clone());
    let want = [
        (
            "12".to_string(),
            "2".to_string(),
            json!({"tag": "mothership", "comments": {"src/a b.rs": [
                {"in_reply_to": "c1", "message": quoted, "unresolved": false, "line": 3},
            ]}}),
        ),
        (
            "12".to_string(),
            "current".to_string(),
            json!({"tag": "mothership", "message": "Thanks."}),
        ),
    ];
    ensure!(reviews == want, "{reviews:#?}");
    Ok(())
}

/// A `comment-added` event on change `number` of `team/app`.
fn comment_added(url: &str, number: u64, topic: &str, email: &str, message: &str) -> Value {
    json!({
        "type": "comment-added",
        "change": {
            "project": "team/app", "branch": "main", "topic": topic, "number": number,
            "url": format!("{url}/c/team/app/+/{number}"), "status": "NEW",
        },
        "patchSet": {"number": 2, "revision": "abc"},
        "author": {"name": "Alice", "email": email, "username": "alice"},
        "comment": message,
        "eventCreatedOn": 1_700_000_000,
    })
}

/// A review by `email` on patch set 2: its message, and comments by path.
fn review(
    email: &str,
    message: &str,
    comments: &[(&str, &str, Option<u64>, &str)],
) -> (Value, Value) {
    let author = json!({"_account_id": 1, "name": "Alice", "email": email});
    let date = "2026-11-07 10:05:00.000000000";
    let messages = json!([{"id": "m1", "author": author, "date": date, "message": message, "_revision_number": 2}]);
    let mut by_path = serde_json::Map::new();
    for (id, path, line, text) in comments {
        let list = by_path.entry(*path).or_insert_with(|| json!([]));
        list.as_array_mut().expect("a list").push(json!({
            "id": id, "author": author, "updated": date, "line": line, "patch_set": 2,
            "message": text, "unresolved": true,
        }));
    }
    (messages, Value::Object(by_path))
}

/// A trusted reviewer's comments that mention the agent reach the session of the issue whose
/// key is the change's topic, with the command that answers each one, and the change the
/// session mentions is linked on it. A wrong token is refused;
/// untrusted authors, reviews that do not mention the agent and other topics are ignored.
async fn gerrit_review_continues_the_session_of_its_topic(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        repos: vec![("app", ORIGIN)],
        gerrit: true,
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    h.logged("gerrit feedback accepted for team/app").await?;
    let gerrit = h.gerrit.as_ref().context("gerrit is on")?;
    let url = gerrit.url.clone();
    let issue = issue(31, "Ship it.");
    h.mock.add_issue(&issue);
    ensure!(h.linear(created("s31", &issue, "context of SH-31")).await? == 200);
    h.activity("s31", "response", 1).await?;

    let message = "Patch Set 2:\n\n(2 comments)";
    let (messages, comments) = review(
        GERRIT_TRUSTED,
        message,
        &[
            ("c1", "src/lib.rs", Some(3), "@acme-bot rename this"),
            ("c2", "/PATCHSET_LEVEL", None, "Looks close."),
        ],
    );
    gerrit.set_review(12, messages, comments);
    let aside_message = "Patch Set 2:\n\n(1 comment)";
    let (messages, comments) = review(
        GERRIT_TRUSTED,
        aside_message,
        &[("c3", "src/lib.rs", Some(1), "aside to a reviewer")],
    );
    gerrit.set_review(13, messages, comments);

    let event = comment_added(&url, 12, "SH-31", GERRIT_TRUSTED, message);
    ensure!(h.gerrit_event("wrong", &event).await? == 401);
    let outsider = comment_added(&url, 12, "SH-31", "mallory@example.com", message);
    ensure!(h.gerrit_event(GERRIT_TOKEN, &outsider).await? == 200);
    let other_topic = comment_added(&url, 12, "SH-99", GERRIT_TRUSTED, message);
    ensure!(h.gerrit_event(GERRIT_TOKEN, &other_topic).await? == 200);
    let aside = comment_added(&url, 13, "SH-31", GERRIT_TRUSTED, aside_message);
    ensure!(h.gerrit_event(GERRIT_TOKEN, &aside).await? == 200);
    let created = json!({"type": "patchset-created", "change": {"topic": "SH-31"}});
    ensure!(h.gerrit_event(GERRIT_TOKEN, &created).await? == 200);
    ensure!(h.gerrit_event(GERRIT_TOKEN, &event).await? == 200);

    let call = h.prompt_with("rename this").await?;
    let prompt = call["prompt"].as_str().unwrap_or_default();
    let head = format!("Review from Alice on Gerrit change {url}/c/team/app/+/12 (patch set 2)");
    ensure!(prompt.starts_with(&head), "{prompt}");
    for part in [
        "At src/lib.rs:3 (patch set 2); answer with `gerrit-reply 12 2 c1 src/lib.rs 3`:",
        "On the change (patch set 2); answer with `gerrit-reply 12 2 c2 /PATCHSET_LEVEL`:",
        "Looks close.",
    ] {
        ensure!(prompt.contains(part), "no {part:?} in {prompt}");
    }
    h.activity("s31", "response", 2).await?;
    // The fake agent answers `Done: <prompt>`, so the change URL is in its response.
    let link = json!({"label": "team/app/+/12", "url": format!("{url}/c/team/app/+/12")});
    let update = eventually("agentSessionUpdate", || {
        h.mock.read(|r| r.session_updates.first().cloned())
    })
    .await?;
    ensure!(
        update == json!({"id": "s31", "input": {"addedExternalUrls": [link]}}),
        "{update}"
    );
    ensure!(
        h.prompts_with("rename this").len() == 1,
        "outsider or another topic heard"
    );
    ensure!(
        h.prompts_with("aside to a reviewer").is_empty(),
        "review without a mention heard"
    );
    Ok(())
}
