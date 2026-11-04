//! Sessions mothership opens itself: one per GitHub pull request a mention asks for, on the
//! issue the pull request belongs to, so feedback on two pull requests of one issue never
//! shares a session.

use super::{Linear, start::start_turn};
use crate::{
    app::App,
    store::{PullRequestRef, SessionRec},
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

/// How long opening a session may hold the lock `created` webhooks wait on.
const CREATE_TIMEOUT: Duration = Duration::from_secs(30);

/// What a session for a pull request starts from.
pub struct Opening<'a> {
    /// The issue as [`Linear::issue_by_key`] returns it.
    pub issue: &'a Value,
    /// Repository name in `repos.json`.
    pub repo: String,
    /// The pull request's head branch, which the session's worktree checks out.
    pub branch: String,
    pub pull_request: PullRequestRef,
    /// The pull request's title, for the session's.
    pub title: &'a str,
    pub prompt: String,
}

impl Linear {
    /// The issue `key` (`ACME-593`) with the fields a session records; `None` when Linear has no
    /// such issue.
    pub async fn issue_by_key(&self, app: &App, key: &str) -> Result<Option<Value>> {
        let data = self
            .graphql(
                app,
                "query($id: String!) { issue(id: $id) { id identifier title url description } }",
                json!({ "id": key }),
            )
            .await?;
        Ok(Some(data["issue"].clone()).filter(|i| i["id"].is_string()))
    }

    /// Opens a Linear session on the issue for the pull request, records it, and starts its
    /// first turn with the opening's prompt. Returns the session id.
    pub async fn open_for_pull_request(
        &self,
        app: &Arc<App>,
        opening: Opening<'_>,
    ) -> Result<String> {
        let Opening {
            issue,
            repo,
            branch,
            pull_request: pr,
            title,
            prompt,
        } = opening;
        let field = |k: &str| issue[k].as_str().unwrap_or_default().to_string();
        let label = format!("{}#{}", pr.repo, pr.number);
        let sid = {
            // Linear may send the session's `created` webhook before this call returns; it
            // waits here, and finds the session recorded as opened by mothership.
            let guard = self.opening.lock().await;
            // A second mention that arrived while the first one opened the session joins it.
            let existing = app.store.read(|s| {
                s.sessions
                    .iter()
                    .find(|(_, r)| r.pull_request.as_ref().is_some_and(|own| own.is(&pr)))
                    .map(|(sid, _)| sid.clone())
            });
            if let Some(sid) = existing {
                drop(guard);
                start_turn(app, &sid, issue, prompt, None).await;
                return Ok(sid);
            }
            // Every `created` webhook waits on this lock, so a call that hangs must not hold it.
            // Should Linear still create the session after the timeout, its `created` webhook
            // starts it as any other.
            let create = self.graphql(
                app,
                "mutation($input: AgentSessionCreateOnIssue!) { \
                 agentSessionCreateOnIssue(input: $input) { success agentSession { id } } }",
                json!({ "input": {
                    "issueId": field("id"),
                    "externalUrls": [{ "label": label, "url": pr.url }],
                } }),
            );
            let data = tokio::time::timeout(CREATE_TIMEOUT, create)
                .await
                .context("agentSessionCreateOnIssue timed out")??;
            let sid = data["agentSessionCreateOnIssue"]["agentSession"]["id"]
                .as_str()
                .context("agentSessionCreateOnIssue returned no session")?
                .to_string();
            app.store.update(|s| {
                s.sessions.insert(
                    sid.clone(),
                    SessionRec {
                        issue_id: field("id"),
                        identifier: field("identifier"),
                        title: field("title"),
                        url: field("url"),
                        repo: Some(repo),
                        mode_pending: true,
                        branch: Some(branch),
                        pull_requests: vec![pr.url.clone()],
                        pull_request: Some(pr.clone()),
                        ..SessionRec::default()
                    },
                );
            });
            sid
        };
        if let Err(e) = self.set_summary(app, &sid, &summary(&pr, title)).await {
            tracing::warn!("[{sid}] naming the session after {label} failed: {e:#}");
        }
        start_turn(app, &sid, issue, prompt, None).await;
        Ok(sid)
    }

    /// Whether mothership opened the session `sid` itself, so its `created` webhook starts
    /// nothing: the turn is already under way.
    pub(super) async fn opened_here(&self, app: &App, sid: &str) -> bool {
        let _guard = self.opening.lock().await;
        app.store.read(|s| {
            s.sessions
                .get(sid)
                .is_some_and(|r| r.pull_request.is_some())
        })
    }

    async fn set_summary(&self, app: &App, sid: &str, summary: &str) -> Result<()> {
        let data = self
            .graphql(
                app,
                "mutation($id: String!, $input: AgentSessionUpdateInput!) { \
                 agentSessionUpdate(id: $id, input: $input) { success } }",
                json!({ "id": sid, "input": { "summary": summary } }),
            )
            .await?;
        anyhow::ensure!(
            data["agentSessionUpdate"]["success"] == true,
            "agentSessionUpdate did not succeed"
        );
        Ok(())
    }
}

/// The session's title: `PR #7: <title>` on one line, within Linear's 255 characters.
fn summary(pr: &PullRequestRef, title: &str) -> String {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let full = match title.as_str() {
        "" => format!("PR #{}", pr.number),
        t => format!("PR #{}: {t}", pr.number),
    };
    full.chars().take(255).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_is_one_short_line() {
        let pr = PullRequestRef {
            number: 7,
            ..PullRequestRef::default()
        };
        assert_eq!(summary(&pr, "Fix\n login  page "), "PR #7: Fix login page");
        assert_eq!(summary(&pr, " \n"), "PR #7");
        let long = summary(&pr, &"é".repeat(300));
        assert_eq!(long.chars().count(), 255);
    }
}
