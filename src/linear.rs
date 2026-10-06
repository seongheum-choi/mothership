use crate::{App, Res, Tokens};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;

const GRAPHQL: &str = "https://api.linear.app/graphql";
const TOKEN: &str = "https://api.linear.app/oauth/token";

/// `Linear-Signature` is hex(HMAC-SHA256(secret, raw body)); a missing `webhookTimestamp`,
/// or one more than a minute off, is treated as a replay. Same rules as `@linear/sdk` LinearWebhookClient.
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
    let ts = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["webhookTimestamp"].as_u64());
    ts.is_some_and(|ts| now_ms.abs_diff(ts) <= 60_000)
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

impl App {
    /// GraphQL call that refreshes the OAuth token once on an auth failure (tokens expire after 24h).
    pub async fn graphql(&self, query: &str, variables: Value) -> Res<Value> {
        for attempt in 0..2 {
            let token = self.store.lock().unwrap().linear.access_token.clone();
            let res = self
                .http
                .post(GRAPHQL)
                .bearer_auth(&token)
                .json(&json!({ "query": query, "variables": variables }))
                .send()
                .await?;
            let status = res.status();
            let text = res.text().await?;
            let body: Value = serde_json::from_str(&text).unwrap_or_default();
            let auth_error = body["errors"].to_string().contains("AUTHENTICATION_ERROR");
            if attempt == 0 && (status == reqwest::StatusCode::UNAUTHORIZED || auth_error) {
                self.refresh(&token).await?;
                continue;
            }
            if !status.is_success() && body.get("errors").is_none() {
                let snippet: String = text.chars().take(200).collect();
                return Err(format!("Linear GraphQL: HTTP {status}: {snippet}").into());
            }
            if let Some(errors) = body.get("errors") {
                return Err(format!("Linear GraphQL: {errors}").into());
            }
            return Ok(body["data"].clone());
        }
        unreachable!()
    }

    /// Refreshes unless another task already replaced `stale` (Linear rotates refresh tokens,
    /// so two concurrent refreshes would invalidate each other).
    async fn refresh(&self, stale: &str) -> Res<()> {
        let _guard = self.refresh_lock.lock().await;
        let current = self.store.lock().unwrap().linear.clone();
        if current.access_token != stale {
            return Ok(());
        }
        let mut tokens = self
            .token_request(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &current.refresh_token),
            ])
            .await?;
        if tokens.refresh_token.is_empty() {
            tokens.refresh_token = current.refresh_token; // not rotated this time
        }
        self.update(|s| s.linear = tokens);
        eprintln!("Linear token refreshed");
        Ok(())
    }

    pub async fn exchange_code(&self, code: &str) -> Res<Tokens> {
        let redirect_uri = format!("{}/callback", self.cfg.base_url);
        self.token_request(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &redirect_uri),
        ])
        .await
    }

    async fn token_request(&self, params: &[(&str, &str)]) -> Res<Tokens> {
        let mut form = vec![
            ("client_id", self.cfg.client_id.as_str()),
            ("client_secret", self.cfg.client_secret.as_str()),
        ];
        form.extend_from_slice(params);
        let res = self.http.post(TOKEN).form(&form).send().await?;
        if !res.status().is_success() {
            return Err(format!(
                "Linear token endpoint: {} {}",
                res.status(),
                res.text().await?
            )
            .into());
        }
        let t: TokenResponse = res.json().await?;
        Ok(Tokens {
            access_token: t.access_token,
            refresh_token: t.refresh_token.unwrap_or_default(),
        })
    }

    /// Posts an agent activity; failures are logged, never fatal to the session.
    pub async fn activity(&self, session_id: &str, content: Value, ephemeral: bool) {
        let query = "mutation($input: AgentActivityCreateInput!) { agentActivityCreate(input: $input) { success } }";
        let input =
            json!({ "agentSessionId": session_id, "content": content, "ephemeral": ephemeral });
        if let Err(e) = self.graphql(query, json!({ "input": input })).await {
            eprintln!("[{session_id}] activity failed: {e}");
        }
    }

    /// Linear's suggested git branch name for the issue (the "Copy git branch name" value).
    pub async fn branch_name(&self, issue_id: &str) -> Res<String> {
        let data = self
            .graphql(
                "query($id: String!) { issue(id: $id) { branchName } }",
                json!({ "id": issue_id }),
            )
            .await?;
        data["issue"]["branchName"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| "issue has no branchName".into())
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
}
