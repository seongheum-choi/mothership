//! Linear GraphQL with the stored OAuth token, refreshed when it expires, and the agent session
//! mutations sessions make.

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
    let payload = json!({ "query": query, "variables": variables });
    let send = || {
        app.http
            .post(format!("{}/graphql", app.cfg.linear.api_url))
            .bearer_auth(token)
            .json(&payload)
            .send()
    };
    // A pooled connection Linear has already closed fails with a reset before the request is
    // read; the retry gets a fresh connection.
    let res = match send().await {
        Err(e) if e.is_request() => send().await?,
        res => res?,
    };
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

    /// Updates agent session `id` with `input`, an `AgentSessionUpdateInput`.
    pub(super) async fn update_session(&self, app: &App, id: &str, input: Value) -> Result<()> {
        let data = self
            .graphql(
                app,
                "mutation($id: String!, $input: AgentSessionUpdateInput!) { \
                 agentSessionUpdate(id: $id, input: $input) { success } }",
                json!({ "id": id, "input": input }),
            )
            .await?;
        if data["agentSessionUpdate"]["success"] != true {
            bail!("agentSessionUpdate did not succeed");
        }
        Ok(())
    }
}
