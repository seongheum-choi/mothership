//! GitHub pull requests a turn mentions, added to the Linear session's external URLs so the
//! session shows them and Linear links the session to each pull request once it is synced.

use super::Linear;
use crate::{app::App, session::Outcome};
use anyhow::{Result, bail};
use serde_json::json;

/// A pull request URL without fragment or query, and its `owner/repo#n` label.
#[derive(Debug, PartialEq)]
pub(super) struct PullRequest {
    pub url: String,
    pub label: String,
}

const GITHUB: &str = "https://github.com/";

/// Every `https://github.com/<owner>/<repo>/pull/<n>` in `text`, first mention first, once each.
pub(super) fn pull_requests(text: &str) -> Vec<PullRequest> {
    let mut found: Vec<PullRequest> = Vec::new();
    for (start, _) in text.match_indices(GITHUB) {
        if let Some(pr) = parse(&text[start + GITHUB.len()..])
            && !found.iter().any(|f| f.url == pr.url)
        {
            found.push(pr);
        }
    }
    found
}

/// The pull request `rest` (what follows `https://github.com/`) starts with.
fn parse(rest: &str) -> Option<PullRequest> {
    let mut parts = rest.splitn(4, '/');
    let owner = parts.next().filter(|s| is_name(s))?;
    let repo = parts.next().filter(|s| is_name(s))?;
    if parts.next() != Some("pull") {
        return None;
    }
    let tail = parts.next()?;
    let digits = tail.chars().take_while(char::is_ascii_digit).count();
    let number = &tail[..digits];
    // `pull/12abc` is no pull request; `pull/12/files`, `pull/12#r3` and `pull/12.` are. A
    // `.` can end a sentence but never continue a number, unlike in owner and repository names.
    let next = tail[digits..].chars().next();
    if number.is_empty() || number.starts_with('0') || next.is_some_and(continues_number) {
        return None;
    }
    Some(PullRequest {
        url: format!("{GITHUB}{owner}/{repo}/pull/{number}"),
        label: format!("{owner}/{repo}#{number}"),
    })
}

fn is_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_name_char)
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

fn continues_number(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_')
}

impl Linear {
    /// Remembers the pull requests in the running turn's interim text of session `key`.
    pub(super) fn note_pull_requests(&self, key: &str, text: &str) {
        let found = pull_requests(text);
        if found.is_empty() {
            return;
        }
        let mut turns = self.turn_prs.lock().expect("turn_prs lock poisoned");
        let noted = turns.entry(key.to_string()).or_default();
        for pr in found {
            if !noted.contains(&pr) {
                noted.push(pr);
            }
        }
    }

    /// Forgets what an earlier turn of `key` mentioned.
    pub(super) fn start_turn(&self, key: &str) {
        self.turn_prs
            .lock()
            .expect("turn_prs lock poisoned")
            .remove(key);
    }

    /// Adds the pull requests the turn mentioned, its reply included, that the session does
    /// not have yet. A failure is logged and leaves them unrecorded, so a later turn retries.
    pub(super) async fn register_pull_requests(&self, app: &App, key: &str, outcome: &Outcome) {
        let reply = match outcome {
            Outcome::Reply(text) => text.as_str(),
            Outcome::Failed(_) | Outcome::Stopped(_) => "",
        };
        let mut prs = self
            .turn_prs
            .lock()
            .expect("turn_prs lock poisoned")
            .remove(key)
            .unwrap_or_default();
        for pr in pull_requests(reply) {
            if !prs.contains(&pr) {
                prs.push(pr);
            }
        }
        let known = app.store.read(|s| {
            s.sessions
                .get(key)
                .map(|r| r.pull_requests.clone())
                .unwrap_or_default()
        });
        prs.retain(|pr| !known.contains(&pr.url));
        if prs.is_empty() {
            return;
        }
        if let Err(e) = self.add_external_urls(app, key, &prs).await {
            tracing::warn!("[{key}] adding pull request links failed: {e:#}");
            return;
        }
        app.store.update(|s| {
            if let Some(r) = s.sessions.get_mut(key) {
                r.pull_requests.extend(prs.into_iter().map(|pr| pr.url));
            }
        });
    }

    async fn add_external_urls(
        &self,
        app: &App,
        session_id: &str,
        prs: &[PullRequest],
    ) -> Result<()> {
        let urls: Vec<_> = prs
            .iter()
            .map(|pr| json!({ "label": pr.label, "url": pr.url }))
            .collect();
        let data = self
            .graphql(
                app,
                "mutation($id: String!, $input: AgentSessionUpdateInput!) { \
                 agentSessionUpdate(id: $id, input: $input) { success } }",
                json!({ "id": session_id, "input": { "addedExternalUrls": urls } }),
            )
            .await?;
        if data["agentSessionUpdate"]["success"] != true {
            bail!("agentSessionUpdate did not succeed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(text: &str) -> Vec<String> {
        pull_requests(text).into_iter().map(|pr| pr.url).collect()
    }

    #[test]
    fn a_full_stop_after_the_number_ends_the_url() {
        assert_eq!(
            urls("Opened https://github.com/acme/app/pull/12."),
            ["https://github.com/acme/app/pull/12"]
        );
        assert_eq!(
            urls("See https://github.com/acme/app/pull/7. Done."),
            ["https://github.com/acme/app/pull/7"]
        );
        assert_eq!(
            urls("https://github.com/acme/app/pull/12abc"),
            Vec::<String>::new()
        );
        assert_eq!(
            urls("https://github.com/acme/app/pull/12-x"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn finds_pull_requests_without_fragment_or_query() {
        let text = "Opened https://github.com/acme/app/pull/12. Review: \
                    https://github.com/acme/app/pull/12#discussion_r3, \
                    (https://github.com/my-org/web.site/pull/7?tab=files) and \
                    https://github.com/acme/app/pull/12/files";
        assert_eq!(
            urls(text),
            [
                "https://github.com/acme/app/pull/12",
                "https://github.com/my-org/web.site/pull/7",
            ]
        );
        assert_eq!(pull_requests(text)[1].label, "my-org/web.site#7");
    }

    #[test]
    fn ignores_what_is_not_a_pull_request() {
        for text in [
            "https://github.com/acme/app/issues/3",
            "https://github.com/acme/app/pull/",
            "https://github.com/acme/app/pull/12abc",
            "https://github.com/acme/app/pull/012",
            "https://github.com/acme/app/pulls",
            "https://github.com//app/pull/1",
            "http://github.com/acme/app/pull/1",
            "https://gitlab.com/acme/app/pull/1",
        ] {
            assert_eq!(urls(text), Vec::<String>::new(), "{text}");
        }
    }
}
