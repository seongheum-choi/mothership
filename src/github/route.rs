//! Which session a piece of GitHub feedback continues, and starting its turn.

use super::{
    GitHub,
    feedback::{Feedback, Head},
    guard::{Allowance, PROMPTS_PER_WINDOW},
};
use crate::{
    app::App,
    sessions::{self, Found},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{sync::Arc, time::Instant};

pub(super) async fn handle(app: Arc<App>, feedback: Feedback) {
    let github = app
        .github
        .as_ref()
        .expect("github handler runs only when enabled");
    let head = match &feedback.head {
        Some(head) => head.clone(),
        None => match fetch_head(github, &feedback.repo, feedback.number).await {
            Ok(head) => head,
            Err(e) => {
                tracing::warn!(
                    "github {}: head branch lookup failed: {e:#}",
                    feedback.pr_url
                );
                return;
            }
        },
    };
    if !head.same_repo(&feedback.repo) {
        tracing::info!(
            "github {} on {}: head is in {}, ignored",
            feedback.pr_url,
            head.branch,
            head.repo
        );
        return;
    }
    let names = github.names(&feedback.repo);
    let sid = match sessions::for_branch(&app.store, &app.cfg.repos, &head.branch, &names).await {
        Some(Found::Open(sid)) => sid,
        Some(Found::Closed(sid)) => {
            tracing::info!(
                "[{sid}] github {} on {}: the session's issue is closed, ignored",
                feedback.pr_url,
                head.branch
            );
            return;
        }
        None => {
            tracing::info!(
                "github {} on {}: no session for this branch in this repository, ignored",
                feedback.pr_url,
                head.branch
            );
            return;
        }
    };
    let allowed = github
        .budget
        .lock()
        .expect("prompt budget poisoned")
        .take(&sid, Instant::now());
    match allowed {
        Allowance::Granted => {}
        Allowance::Denied { notify: false } => {
            tracing::info!(
                "[{sid}] github feedback paused, @{} ignored",
                feedback.author
            );
            return;
        }
        Allowance::Denied { notify: true } => {
            tracing::warn!(
                "[{sid}] {PROMPTS_PER_WINDOW} GitHub prompts within the hour; pausing GitHub feedback"
            );
            app.linear.stop(&sid, None);
            let body = format!(
                "Paused GitHub feedback: this session got {PROMPTS_PER_WINDOW} prompts from GitHub \
                 within an hour, which looks like the agent answering its own comments. The \
                 running turn was stopped, and GitHub comments on this PR are ignored until the \
                 hour is up. Prompt here to continue."
            );
            app.linear
                .surface
                .activity(&app, &sid, json!({ "type": "error", "body": body }), false)
                .await;
            return;
        }
    }
    tracing::info!(
        "[{sid}] github {} from @{} on {}",
        feedback.kind.label(),
        feedback.author,
        head.branch
    );
    sessions::mark_prompted(&app.store, &sid);
    app.linear.submit(&app, &sid, feedback.prompt(), ());
}

/// `issue_comment` payloads describe the PR as an issue, without its head.
async fn fetch_head(github: &GitHub, repo: &str, number: u64) -> Result<Head> {
    let clone = github
        .clone_of(repo)
        .with_context(|| format!("no repository has origin {repo}"))?;
    let out = tokio::process::Command::new("gh")
        .args(["api", &format!("repos/{repo}/pulls/{number}")])
        .current_dir(clone)
        .output()
        .await
        .context("running gh")?;
    if !out.status.success() {
        bail!("gh api: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let pr: Value = serde_json::from_slice(&out.stdout).context("gh api output is not JSON")?;
    Head::of(&pr).context("pull request has no head branch")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{github::tests::github, store::SessionRec};
    use std::collections::HashMap;

    fn open(found: Option<Found>) -> Option<String> {
        match found? {
            Found::Open(sid) => Some(sid),
            Found::Closed(sid) => panic!("{sid} is closed"),
        }
    }

    #[test]
    fn same_branch_name_in_another_repository_is_not_mixed_up() {
        let rec = |repo: Option<&str>, prompted_at| SessionRec {
            repo: repo.map(Into::into),
            branch: Some("fix-login".into()),
            prompted_at,
            ..SessionRec::default()
        };
        let sessions = HashMap::from([
            ("app".to_string(), rec(Some("app"), 1)),
            ("lib".to_string(), rec(Some("Lib"), 5)),
            ("unknown".to_string(), rec(None, 9)),
        ]);
        let gh = github();
        let find = |full_name| open(sessions::find(&sessions, "fix-login", &gh.names(full_name)));
        assert_eq!(find("o/r").as_deref(), Some("app"));
        assert_eq!(find("o/lib").as_deref(), Some("lib"), "names ignore case");
        assert_eq!(find("o/other"), None);
    }
}
