//! An issue's agent sessions as Linear has them: each session, the root comment of its thread,
//! the last report among all its activities, and a reply posted under that root comment.

use super::Linear;
use crate::app::App;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// Linear's page size limit for connections.
const PAGE: u32 = 250;
/// Pages read before giving up on a session's activities: 50,000 activities.
const MAX_PAGES: usize = 200;
/// Activity types that end a turn or ask for input: what the agent last reported.
const REPORTS: [&str; 3] = [
    "AgentActivityResponseContent",
    "AgentActivityErrorContent",
    "AgentActivityElicitationContent",
];

const ISSUE_SESSIONS: &str = "query($id: String!) { issue(id: $id) { id identifier title url \
     agentSessions(first: 50) { nodes { id status createdAt comment { id parent { id } } } } } }";

const ACTIVITIES: &str = "query($id: String!, $first: Int!, $after: String) { \
     agentSession(id: $id) { activities(first: $first, after: $after) { \
     pageInfo { hasNextPage endCursor } nodes { createdAt content { __typename \
     ... on AgentActivityResponseContent { body } ... on AgentActivityErrorContent { body } \
     ... on AgentActivityElicitationContent { body } } } } } }";

const COMMENT_CREATE: &str = "mutation($input: CommentCreateInput!) { \
     commentCreate(input: $input) { success comment { id } } }";

/// An issue and its agent sessions, newest first.
pub(super) struct IssueSessions {
    pub issue: Value,
    pub sessions: Vec<Value>,
}

/// The activity a session last reported with.
#[derive(Debug, PartialEq)]
pub(super) struct Report {
    pub at: String,
    /// `response`, `error` or `elicitation`.
    pub kind: String,
    pub body: String,
}

impl Linear {
    /// The issue `key` (an identifier like `SH-1`, or an id) and its agent sessions.
    pub(super) async fn issue_sessions(&self, app: &App, key: &str) -> Result<IssueSessions> {
        let data = self
            .graphql(app, ISSUE_SESSIONS, json!({ "id": key }))
            .await?;
        let issue = data["issue"].clone();
        if issue.is_null() {
            bail!("Linear has no issue {key}");
        }
        let mut sessions = issue["agentSessions"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        newest_first(&mut sessions);
        Ok(IssueSessions { issue, sessions })
    }

    /// The last report among all of session `sid`'s activities, reading every page: Linear's
    /// order within a page is not the order they were posted in, and the newest may be on the
    /// last page.
    pub(super) async fn last_report(&self, app: &App, sid: &str) -> Result<Option<Report>> {
        let mut after: Option<String> = None;
        let mut last = None;
        for _ in 0..MAX_PAGES {
            let vars = json!({ "id": sid, "first": PAGE, "after": after });
            let data = self.graphql(app, ACTIVITIES, vars).await?;
            let page = &data["agentSession"]["activities"];
            let nodes = page["nodes"].as_array().map_or(&[][..], Vec::as_slice);
            last = later(last, latest_report(nodes));
            if page["pageInfo"]["hasNextPage"] != true {
                return Ok(last);
            }
            after = page["pageInfo"]["endCursor"].as_str().map(String::from);
            if after.is_none() {
                bail!("Linear said session {sid} has more activities but gave no cursor");
            }
        }
        bail!(
            "session {sid} has more than {} activities",
            PAGE as usize * MAX_PAGES
        )
    }

    /// Posts `body` on the issue as a reply to `parent`. Returns the new comment's id.
    pub(super) async fn reply_in_thread(
        &self,
        app: &App,
        issue_id: &str,
        parent: &str,
        body: &str,
    ) -> Result<String> {
        let input = json!({ "issueId": issue_id, "parentId": parent, "body": body });
        let data = self
            .graphql(app, COMMENT_CREATE, json!({ "input": input }))
            .await?;
        if data["commentCreate"]["success"] != true {
            bail!("commentCreate did not succeed");
        }
        data["commentCreate"]["comment"]["id"]
            .as_str()
            .map(String::from)
            .context("commentCreate returned no comment id")
    }
}

/// Sorts sessions by `createdAt`, newest first; Linear's timestamps are uniform ISO 8601, so
/// they sort as strings.
fn newest_first(sessions: &mut [Value]) {
    sessions.sort_by(|a, b| {
        let at = |s: &Value| s["createdAt"].as_str().unwrap_or_default().to_string();
        at(b).cmp(&at(a))
    });
}

/// The top-level comment of a session's thread: Linear's marker comment for a delegated
/// session, or the comment whose mention opened it. A mention in a reply opens a session on
/// that reply, whose thread is its parent's.
pub(super) fn root_comment(session: &Value) -> Option<&str> {
    let comment = &session["comment"];
    comment["parent"]["id"]
        .as_str()
        .or_else(|| comment["id"].as_str())
}

/// The newest report among `nodes`.
fn latest_report(nodes: &[Value]) -> Option<Report> {
    nodes
        .iter()
        .filter_map(|node| {
            let kind = node["content"]["__typename"].as_str()?;
            REPORTS.contains(&kind).then_some(())?;
            Some(Report {
                at: node["createdAt"].as_str()?.to_string(),
                kind: kind
                    .trim_start_matches("AgentActivity")
                    .trim_end_matches("Content")
                    .to_lowercase(),
                body: node["content"]["body"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .max_by(|a, b| a.at.cmp(&b.at))
}

fn later(a: Option<Report>, b: Option<Report>) -> Option<Report> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.at > a.at { b } else { a }),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(at: &str, kind: &str, body: &str) -> Value {
        json!({ "createdAt": at, "content": { "__typename": kind, "body": body } })
    }

    #[test]
    fn the_latest_report_wins_whatever_the_page_order() {
        let nodes = [
            node(
                "2026-11-24T10:00:00.000Z",
                "AgentActivityResponseContent",
                "old",
            ),
            node(
                "2026-11-24T12:00:00.000Z",
                "AgentActivityThoughtContent",
                "thinking",
            ),
            node(
                "2026-11-24T11:00:00.000Z",
                "AgentActivityErrorContent",
                "failed",
            ),
            node(
                "2026-11-24T09:00:00.000Z",
                "AgentActivityElicitationContent",
                "which?",
            ),
        ];
        let report = latest_report(&nodes).unwrap();
        assert_eq!(
            report,
            Report {
                at: "2026-11-24T11:00:00.000Z".into(),
                kind: "error".into(),
                body: "failed".into(),
            }
        );
        let thoughts = [node(
            "2026-11-24T12:00:00.000Z",
            "AgentActivityThoughtContent",
            "x",
        )];
        assert_eq!(latest_report(&thoughts), None);
    }

    #[test]
    fn a_later_page_can_hold_an_older_report() {
        let report = |at: &str| latest_report(&[node(at, "AgentActivityResponseContent", at)]);
        let first = report("2026-11-24T12:00:00.000Z");
        let second = report("2026-11-24T10:00:00.000Z");
        assert_eq!(
            later(first, second).unwrap().body,
            "2026-11-24T12:00:00.000Z"
        );
        let none = later(None, report("2026-11-24T10:00:00.000Z"));
        assert_eq!(none.unwrap().body, "2026-11-24T10:00:00.000Z");
    }

    #[test]
    fn the_root_comment_is_the_top_of_the_thread() {
        let delegated = json!({ "comment": { "id": "root", "parent": null } });
        assert_eq!(root_comment(&delegated), Some("root"));
        let in_reply = json!({ "comment": { "id": "mention", "parent": { "id": "top" } } });
        assert_eq!(root_comment(&in_reply), Some("top"));
        assert_eq!(root_comment(&json!({ "comment": null })), None);
    }

    #[test]
    fn sessions_sort_newest_first() {
        let mut sessions = vec![
            json!({ "id": "a", "createdAt": "2026-11-24T10:00:00.000Z" }),
            json!({ "id": "c", "createdAt": "2026-11-24T12:00:00.000Z" }),
            json!({ "id": "b", "createdAt": "2026-11-24T11:00:00.000Z" }),
        ];
        newest_first(&mut sessions);
        let ids: Vec<&str> = sessions.iter().filter_map(|s| s["id"].as_str()).collect();
        assert_eq!(ids, ["c", "b", "a"]);
    }
}
