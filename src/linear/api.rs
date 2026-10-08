//! Linear GraphQL with the stored OAuth token, refreshed when it expires, and the issue
//! queries and mutations sessions make.

use super::{
    Linear,
    pin::{VIEWER_ORGANIZATION, Workspace},
};
use crate::{app::App, store::Tokens};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
}

/// One GraphQL request with `token`; `None` when Linear rejects the token.
async fn request(app: &App, token: &str, query: &str, variables: Value) -> Result<Option<Value>> {
    let res = app
        .http
        .post(format!("{}/graphql", app.cfg.linear.api_url))
        .bearer_auth(token)
        .json(&json!({ "query": query, "variables": variables }))
        .send()
        .await?;
    let status = res.status();
    let text = res.text().await?;
    let body: Value = serde_json::from_str(&text).unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED
        || body["errors"].to_string().contains("AUTHENTICATION_ERROR")
    {
        return Ok(None);
    }
    if let Some(errors) = body.get("errors") {
        bail!("Linear GraphQL: {errors}");
    }
    if !status.is_success() {
        let snippet: String = text.chars().take(200).collect();
        bail!("Linear GraphQL: HTTP {status}: {snippet}");
    }
    Ok(Some(body["data"].clone()))
}

/// The workspace `token` belongs to, without refreshing it.
pub(super) async fn workspace_of(app: &App, token: &str) -> Result<Workspace> {
    let data = request(app, token, VIEWER_ORGANIZATION, json!({}))
        .await?
        .context("Linear rejected the token")?;
    Workspace::from_viewer(&data)
}

impl Linear {
    /// GraphQL call that refreshes the OAuth token once on an auth failure (tokens expire after 24h).
    pub(super) async fn graphql(&self, app: &App, query: &str, variables: Value) -> Result<Value> {
        let token = app.store.read(|s| s.linear.access_token.clone());
        if let Some(data) = request(app, &token, query, variables.clone()).await? {
            return Ok(data);
        }
        self.refresh(app, &token).await?;
        let token = app.store.read(|s| s.linear.access_token.clone());
        request(app, &token, query, variables)
            .await?
            .context("Linear GraphQL: token rejected right after a refresh")
    }

    /// Looks up the stored token's workspace and pins it. Errors when it is not the
    /// configured `LINEAR_WORKSPACE`.
    pub async fn pin_current(&self, app: &App) -> Result<()> {
        let ws = self.current_workspace(app).await?;
        self.pin(app, ws)
    }

    /// The workspace of the stored token, refreshing the token if it has expired.
    async fn current_workspace(&self, app: &App) -> Result<Workspace> {
        let data = self.graphql(app, VIEWER_ORGANIZATION, json!({})).await?;
        Workspace::from_viewer(&data)
    }

    /// Refreshes unless another task already replaced `stale`. Linear rotates refresh
    /// tokens, so two concurrent refreshes would invalidate each other.
    async fn refresh(&self, app: &App, stale: &str) -> Result<()> {
        let _guard = self.refresh_lock.lock().await;
        let current = app.store.read(|s| s.linear.clone());
        if current.access_token != stale {
            return Ok(());
        }
        let mut tokens = self
            .token_request(
                app,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &current.refresh_token),
                ],
            )
            .await?;
        if tokens.refresh_token.is_empty() {
            tokens.refresh_token = current.refresh_token; // not rotated this time
        }
        // A token from another workspace never reaches the store. When the lookup itself fails,
        // store the pair anyway: Linear has already rotated the refresh token, so dropping the
        // new pair would leave none that works, and a refresh grant cannot change workspace.
        match workspace_of(app, &tokens.access_token).await {
            Ok(ws) => self
                .pin(app, ws)
                .context("refreshed Linear token kept out of the store")?,
            Err(e) => tracing::warn!("workspace lookup after token refresh failed: {e:#}"),
        }
        app.store.update(|s| s.linear = tokens);
        tracing::info!("Linear token refreshed");
        Ok(())
    }

    pub(super) async fn token_request(&self, app: &App, params: &[(&str, &str)]) -> Result<Tokens> {
        let mut form = vec![
            ("client_id", app.cfg.linear.client_id.as_str()),
            ("client_secret", app.cfg.linear.client_secret.as_str()),
        ];
        form.extend_from_slice(params);
        let res = app
            .http
            .post(format!("{}/oauth/token", app.cfg.linear.api_url))
            .form(&form)
            .send()
            .await?;
        if !res.status().is_success() {
            bail!(
                "Linear token endpoint: {} {}",
                res.status(),
                res.text().await?
            );
        }
        let t: TokenResponse = res.json().await?;
        Ok(Tokens {
            access_token: t.access_token,
            refresh_token: t.refresh_token.unwrap_or_default(),
        })
    }

    /// Posts an agent activity. Failures are logged, never fatal to the session.
    pub async fn activity(&self, app: &App, session_id: &str, content: Value, ephemeral: bool) {
        let query = "mutation($input: AgentActivityCreateInput!) { agentActivityCreate(input: $input) { success } }";
        let input =
            json!({ "agentSessionId": session_id, "content": content, "ephemeral": ephemeral });
        if let Err(e) = self.graphql(app, query, json!({ "input": input })).await {
            tracing::warn!("[{session_id}] activity failed: {e:#}");
        }
    }

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
