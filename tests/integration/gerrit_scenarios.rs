//! The Gerrit scenarios: pushing for review, answering on Gerrit, and review feedback reaching
//! the session.

use crate::{
    gerrit,
    harness::{Ctx, Harness, Setup, Skipped},
    scenarios::{Scenario, argv, created, flag, issue},
};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{path::Path, process::Stdio};
use tokio::io::AsyncWriteExt;

macro_rules! scenarios {
    ($($name:ident),* $(,)?) => {
        &[$((stringify!($name), (|ctx| Box::pin($name(ctx))) as Scenario)),*]
    };
}

pub const ALL: &[(&str, Scenario)] = scenarios![gerrit_sessions_push_for_review_and_reply,];

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
