//! The issue queries and mutations sessions make: what routing looks at, the branch name, and
//! moving the issue to a started state.

use super::Linear;
use crate::app::App;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

impl Linear {
    /// The issue's description, project, team and labels: what repository routing looks at.
    /// Webhook payloads carry none of the last three.
    pub(super) async fn issue_routing(&self, app: &App, issue_id: &str) -> Result<Value> {
        let data = self
            .graphql(
                app,
                "query($id: String!) { issue(id: $id) { description \
                 project { id name slugId } team { id key name } \
                 labels { nodes { id name } } } }",
                json!({ "id": issue_id }),
            )
            .await?;
        Ok(data["issue"].clone())
    }

    /// Linear's suggested git branch name for the issue ("Copy git branch name").
    pub(super) async fn branch_name(&self, app: &App, issue_id: &str) -> Result<String> {
        let data = self
            .graphql(
                app,
                "query($id: String!) { issue(id: $id) { branchName } }",
                json!({ "id": issue_id }),
            )
            .await?;
        data["issue"]["branchName"]
            .as_str()
            .map(String::from)
            .context("issue has no branchName")
    }

    /// Moves a not-yet-started issue to its team's first `started` state.
    pub(super) async fn start_issue(&self, app: &App, issue_id: &str) -> Result<()> {
        let data = self
            .graphql(
                app,
                "query($id: String!) { issue(id: $id) { state { type } \
                 team { states(filter: { type: { eq: \"started\" } }) { nodes { id type position } } } } }",
                json!({ "id": issue_id }),
            )
            .await?;
        let Some(state_id) = started_state(&data["issue"]) else {
            return Ok(());
        };
        let data = self
            .graphql(
                app,
                "mutation($id: String!, $stateId: String!) { \
                 issueUpdate(id: $id, input: { stateId: $stateId }) { success } }",
                json!({ "id": issue_id, "stateId": state_id }),
            )
            .await?;
        if data["issueUpdate"]["success"] != true {
            bail!("issueUpdate did not succeed");
        }
        Ok(())
    }
}

/// The state to move `issue` to: the lowest-`position` `started` state of its team, but only
/// while the issue sits in triage, backlog or unstarted. Later states are the team's to manage.
fn started_state(issue: &Value) -> Option<&str> {
    if !matches!(
        issue["state"]["type"].as_str(),
        Some("triage" | "backlog" | "unstarted")
    ) {
        return None;
    }
    issue["team"]["states"]["nodes"]
        .as_array()?
        .iter()
        .filter(|s| s["type"] == "started")
        .filter_map(|s| Some((s["position"].as_f64()?, s["id"].as_str()?)))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(state: &str) -> Value {
        json!({
            "state": { "type": state },
            "team": { "states": { "nodes": [
                { "id": "todo", "type": "unstarted", "position": 0.0 },
                { "id": "review", "type": "started", "position": 3.0 },
                { "id": "progress", "type": "started", "position": 1.5 },
                { "id": "done", "type": "completed", "position": 4.0 },
            ] } }
        })
    }

    #[test]
    fn started_state_picks_first_started() {
        for from in ["triage", "backlog", "unstarted"] {
            assert_eq!(started_state(&issue(from)), Some("progress"), "{from}");
        }
    }

    #[test]
    fn started_state_leaves_later_states_alone() {
        for from in ["started", "completed", "canceled", ""] {
            assert_eq!(started_state(&issue(from)), None, "{from}");
        }
    }

    #[test]
    fn started_state_needs_a_started_state() {
        let mut i = issue("backlog");
        i["team"]["states"]["nodes"] =
            json!([{ "id": "todo", "type": "unstarted", "position": 0.0 }]);
        assert_eq!(started_state(&i), None);
    }
}
