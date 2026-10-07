//! Linear API: webhook signatures, OAuth tokens, GraphQL.

use super::Linear;
use crate::{app::App, store::Tokens};
use anyhow::{Context, Result, bail};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;

const GRAPHQL: &str = "https://api.linear.app/graphql";
const TOKEN: &str = "https://api.linear.app/oauth/token";

/// `Linear-Signature` is hex(HMAC-SHA256(secret, raw body)). A missing `webhookTimestamp`,
/// or one more than a minute off, is treated as a replay.
pub fn verify(secret: &str, body: &[u8], signature: &str, now_ms: u64) -> bool {
    let Some(sig) = decode_hex(signature) else {
        return false;
    };
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key length");
    mac.update(body);
    if mac.verify_slice(&sig).is_err() {
        return false;
    }
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["webhookTimestamp"].as_u64())
        .is_some_and(|ts| now_ms.abs_diff(ts) <= 60_000)
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
}

/// The workspace (Linear organization) an OAuth token belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub url_key: String,
    pub name: String,
}

const VIEWER_ORGANIZATION: &str = "query { viewer { organization { id urlKey name } } }";

impl Workspace {
    fn from_viewer(data: &Value) -> Result<Self> {
        let org = &data["viewer"]["organization"];
        let field = |k: &str| {
            org[k]
                .as_str()
                .map(String::from)
                .with_context(|| format!("viewer.organization.{k} missing"))
        };
        Ok(Self {
            id: field("id")?,
            url_key: field("urlKey")?,
            name: field("name")?,
        })
    }

    /// `LINEAR_WORKSPACE` names the workspace by ID or by URL key (`linear.app/<urlKey>`),
    /// the latter case-insensitively since Linear lowercases it.
    pub fn is(&self, setting: &str) -> bool {
        self.id == setting || self.url_key.eq_ignore_ascii_case(setting)
    }
}

impl std::fmt::Display for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}, {})", self.name, self.url_key, self.id)
    }
}

/// One GraphQL request with `token`; `None` when Linear rejects the token.
async fn request(app: &App, token: &str, query: &str, variables: Value) -> Result<Option<Value>> {
    let res = app
        .http
        .post(GRAPHQL)
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
pub async fn workspace_of(app: &App, token: &str) -> Result<Workspace> {
    let data = request(app, token, VIEWER_ORGANIZATION, json!({}))
        .await?
        .context("Linear rejected the token")?;
    Workspace::from_viewer(&data)
}

impl Linear {
    /// GraphQL call that refreshes the OAuth token once on an auth failure (tokens expire after 24h).
    pub async fn graphql(&self, app: &App, query: &str, variables: Value) -> Result<Value> {
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

    /// The workspace of the stored token, refreshing the token if it has expired.
    pub async fn current_workspace(&self, app: &App) -> Result<Workspace> {
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
        // Like /callback, store the token only once its workspace is the pinned one.
        let ws = workspace_of(app, &tokens.access_token)
            .await
            .context("workspace lookup after token refresh")?;
        self.pin(app, ws)
            .context("refreshed Linear token kept out of the store")?;
        app.store.update(|s| s.linear = tokens);
        tracing::info!("Linear token refreshed");
        Ok(())
    }

    pub async fn exchange_code(&self, app: &App, code: &str) -> Result<Tokens> {
        let redirect_uri = format!("{}/callback", app.cfg.base_url);
        self.token_request(
            app,
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", &redirect_uri),
            ],
        )
        .await
    }

    async fn token_request(&self, app: &App, params: &[(&str, &str)]) -> Result<Tokens> {
        let mut form = vec![
            ("client_id", app.cfg.linear.client_id.as_str()),
            ("client_secret", app.cfg.linear.client_secret.as_str()),
        ];
        form.extend_from_slice(params);
        let res = app.http.post(TOKEN).form(&form).send().await?;
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
    pub async fn issue_routing(&self, app: &App, issue_id: &str) -> Result<Value> {
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
    pub async fn branch_name(&self, app: &App, issue_id: &str) -> Result<String> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature() {
        let body = br#"{"webhookTimestamp":1000000}"#;
        // python3 -c 'import hmac,hashlib;print(hmac.new(b"s",b"{\"webhookTimestamp\":1000000}",hashlib.sha256).hexdigest())'
        let sig = "2645a623818ff5efc2f7f75075285d3e6f4ef1825d34f77912c56416571211b4";
        assert!(verify("s", body, sig, 1_030_000));
        assert!(!verify("s", body, sig, 1_070_000), "stale timestamp");
        assert!(!verify("t", body, sig, 1_030_000), "wrong secret");
        assert!(!verify("s", body, "zz", 1_030_000), "not hex");
    }

    #[test]
    fn workspace_matches_id_or_url_key() {
        let data =
            json!({"viewer":{"organization":{"id":"org-1","urlKey":"alean","name":"ALEAN"}}});
        let ws = Workspace::from_viewer(&data).unwrap();
        assert!(ws.is("org-1"));
        assert!(ws.is("alean"));
        assert!(ws.is("ALEAN"), "URL key is case-insensitive");
        assert!(!ws.is("ORG-1"), "IDs are compared exactly");
        assert!(!ws.is("personal"));
        assert!(Workspace::from_viewer(&json!({"viewer":null})).is_err());
    }
}
