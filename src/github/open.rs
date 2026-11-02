//! A mention on a pull request that no session owns: the Linear issue it belongs to, from the
//! pull request's description or its head branch, and a new Linear session for the pull request
//! on that issue, or a reply on GitHub that there is none.

use super::{
    feedback::{Feedback, Head},
    reply::MARKER,
};
use crate::{app::App, linear::Opening};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{path::Path, sync::Arc};

/// Opens a Linear session for the pull request of `feedback` and starts its first turn with
/// the feedback, or answers on GitHub that no Linear issue was found. `names` are the
/// repositories whose origin the pull request is in.
pub(super) async fn open(app: Arc<App>, feedback: Feedback, head: Head, names: &[&str]) {
    let pr_url = &feedback.pr_url;
    let Some(issue) = find_issue(&app, &feedback, &head).await else {
        tracing::info!(
            "github {pr_url} on {}: no Linear issue in its description or branch",
            head.branch
        );
        let text = "I couldn't find the Linear issue for this pull request, so I did not \
                    start. Link the issue in the description \
                    (`https://linear.app/<workspace>/issue/<KEY-123>`) or name the branch after \
                    it (`<key-123>-…`), then mention me again.";
        if let Err(e) = reply(&app, &feedback, text).await {
            tracing::warn!("github {pr_url}: replying that no issue was found failed: {e:#}");
        }
        return;
    };
    let identifier = issue["identifier"].as_str().unwrap_or_default();
    let repo = repo_for(&app, issue["id"].as_str().unwrap_or_default(), names);
    let opening = Opening {
        issue: &issue,
        repo,
        branch: head.branch.clone(),
        pull_request: feedback.pull_request(),
        title: &feedback.pr_title,
        prompt: format!(
            "{}\n\nThis session was opened from GitHub for pull request {pr_url}; your worktree \
             is on its head branch `{}`.\n\n{}",
            issue_context(&issue),
            head.branch,
            feedback.prompt()
        ),
    };
    match app
        .linear
        .surface
        .open_for_pull_request(&app, opening)
        .await
    {
        Ok(sid) => tracing::info!(
            "[{sid}] github {} from @{} on {pr_url}: opened a session on {identifier}",
            feedback.kind.label(),
            feedback.author
        ),
        Err(e) => {
            tracing::warn!("github {pr_url}: opening a session on {identifier} failed: {e:#}");
        }
    }
}

/// The first of the pull request's issue keys that Linear knows.
async fn find_issue(app: &App, feedback: &Feedback, head: &Head) -> Option<Value> {
    for key in issue_keys(&feedback.pr_body, &head.branch) {
        match app.linear.surface.issue_by_key(app, &key).await {
            Ok(Some(issue)) => return Some(issue),
            Ok(None) => {}
            Err(e) => tracing::info!("github {}: issue {key}: {e:#}", feedback.pr_url),
        }
    }
    None
}

/// The repository the new session works in: the one another session of the issue works in,
/// when it is among `names`, else the first of them.
fn repo_for(app: &App, issue_id: &str, names: &[&str]) -> String {
    let used = app.store.read(|s| {
        s.sessions
            .values()
            .filter(|r| r.issue_id == issue_id)
            .find_map(|r| {
                let repo = r.repo.as_deref()?;
                names.iter().find(|n| n.eq_ignore_ascii_case(repo)).copied()
            })
    });
    used.or(names.first().copied())
        .expect("feedback reaches here only from a watched repository")
        .to_string()
}

/// The issue as the agent first sees it; Linear's prompt context comes only with its own
/// `created` event, which a session mothership opens does not wait for.
fn issue_context(issue: &Value) -> String {
    let field = |k: &str| issue[k].as_str().unwrap_or_default();
    format!(
        "{}: {} ({})\n\n{}",
        field("identifier"),
        field("title"),
        field("url"),
        field("description")
    )
}

/// Linear issue keys (`EN-593`) for a pull request: those its description links first, in
/// order, then the one its head branch is named after (`en-593-3`, `nate/en-593-fix`).
fn issue_keys(body: &str, branch: &str) -> Vec<String> {
    const LINK: &str = "linear.app/";
    let mut keys: Vec<String> = Vec::new();
    let linked = body.match_indices(LINK).filter_map(|(i, _)| {
        let mut parts = body[i + LINK.len()..].splitn(3, '/');
        let _workspace = parts.next()?;
        (parts.next()? == "issue").then_some(())?;
        key_at(parts.next()?)
    });
    let named = branch.rsplit('/').next().and_then(key_at);
    for key in linked.chain(named) {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// The issue key `text` starts with, upper-cased: a team key (a letter, then letters or
/// digits), `-`, a number, then nothing or a character that cannot continue the number.
fn key_at(text: &str) -> Option<String> {
    let (team, rest) = text.split_once('-')?;
    let mut team_chars = team.chars();
    if !team_chars.next()?.is_ascii_alphabetic() || !team_chars.all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    let next = rest[digits..].chars().next();
    if digits == 0 || next.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(format!("{}-{}", team.to_ascii_uppercase(), &rest[..digits]))
}

/// A conversation comment on the pull request, marked like `gh-reply`'s so it never comes
/// back as feedback.
async fn reply(app: &App, feedback: &Feedback, text: &str) -> Result<()> {
    let github = app.github.as_ref().context("github feedback is off")?;
    let origins = github.origins();
    let clone: &Path = origins
        .clone_of(&feedback.repo)
        .with_context(|| format!("no repository has origin {}", feedback.repo))?;
    let out = tokio::process::Command::new("gh")
        .args(["pr", "comment", &feedback.number.to_string()])
        .args(["--repo", &feedback.repo, "--body"])
        .arg(format!("{text}\n\n{MARKER}"))
        .current_dir(clone)
        .output()
        .await
        .context("running gh")?;
    if !out.status.success() {
        bail!(
            "gh pr comment: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_come_from_description_links_then_the_branch() {
        let body = "Fixes https://linear.app/acme/issue/en-593/fix-login and \
                    [SH-7](https://linear.app/acme/issue/SH-7). Again \
                    https://linear.app/acme/issue/EN-593#comment-1";
        assert_eq!(
            issue_keys(body, "nate/sh-9-tidy"),
            ["EN-593", "SH-7", "SH-9"]
        );
        assert_eq!(issue_keys("", "en-593-3"), ["EN-593"]);
        assert_eq!(issue_keys("", "sh-212"), ["SH-212"]);
        assert_eq!(issue_keys("", "a1b-2"), ["A1B-2"]);
        assert_eq!(issue_keys("", "nate/en-593"), ["EN-593"]);
        assert_eq!(
            issue_keys("https://linear.app/acme/issue/EN-1", "en-1-x"),
            ["EN-1"],
            "once each"
        );
    }

    #[test]
    fn what_is_not_an_issue_key_is_skipped() {
        for branch in [
            "main",
            "feature/login",
            "1x-2",
            "en-",
            "en-12a",
            "-5",
            "release-v2",
        ] {
            assert_eq!(issue_keys("", branch), Vec::<String>::new(), "{branch}");
        }
        for body in [
            "https://linear.app/acme/project/EN-5",
            "https://linear.app/acme/issue/",
            "https://linear.app/acme",
            "linear.app/issue/EN-5",
        ] {
            assert_eq!(issue_keys(body, "main"), Vec::<String>::new(), "{body}");
        }
    }
}
