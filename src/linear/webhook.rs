//! Linear webhooks: signature and workspace checks, then agent session events and issue
//! changes, handled in the background.

use super::{
    cleanup::{issue_change, on_issue_change},
    pin::admits,
    start::start_turn,
};
use crate::{
    app::App,
    session::{Outcome, Surface},
};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use serde_json::Value;
use std::sync::Arc;

/// `Linear-Signature` is hex(HMAC-SHA256(secret, raw body)). A missing `webhookTimestamp`,
/// or one more than a minute off, is treated as a replay.
fn verify(secret: &str, body: &[u8], signature: &str, now_ms: u64) -> bool {
    if !crate::signature::hmac_sha256_hex_matches(secret, body, signature) {
        return false;
    }
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["webhookTimestamp"].as_u64())
        .is_some_and(|ts| now_ms.abs_diff(ts) <= 60_000)
}

pub(super) async fn webhook(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let signature = headers
        .get("linear-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    if !verify(&app.cfg.linear.webhook_secret, &body, signature, now_ms) {
        return StatusCode::UNAUTHORIZED;
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    let event = payload["type"].as_str().unwrap_or_default();
    let org = payload["organizationId"].as_str();
    let pinned = app.linear.surface.pinned_id();
    if !admits(pinned.as_deref(), org) {
        tracing::warn!(
            "refused Linear webhook {event} from organization {} (pinned: {})",
            org.unwrap_or("none"),
            pinned.as_deref().unwrap_or("none")
        );
        return StatusCode::FORBIDDEN;
    }
    // Linear wants a 200 within 5 seconds; the work happens in the background.
    tokio::spawn(handle(app, payload));
    StatusCode::OK
}

/// Runs only for webhooks `admits` let through, so issue changes in another workspace never
/// stop sessions or remove worktrees here.
async fn handle(app: Arc<App>, p: Value) {
    app.refresh();
    if let Some(change) = issue_change(&p) {
        on_issue_change(&app, change).await;
        return;
    }
    if p["type"] != "AgentSessionEvent" {
        return;
    }
    let (Some(sid), issue) = (
        p["agentSession"]["id"].as_str(),
        &p["agentSession"]["issue"],
    ) else {
        return;
    };
    if issue.is_null() {
        return;
    }
    let (prompt, request) = match p["action"].as_str() {
        Some("created") => {
            // A delegated session starts work on the issue. A mention may be just a question,
            // so the agent moves those issues itself when it starts work (see the prompt).
            if mention_request(&p).is_none()
                && let Some(issue_id) = issue["id"].as_str()
            {
                let (app, sid, issue_id) = (app.clone(), sid.to_string(), issue_id.to_string());
                tokio::spawn(async move {
                    if let Err(e) = app.linear.surface.start_issue(&app, &issue_id).await {
                        tracing::warn!("[{sid}] moving issue to started failed: {e:#}");
                    }
                });
            }
            (
                initial_prompt(&p),
                p["agentSession"]["comment"]["body"].as_str(),
            )
        }
        Some("prompted") if p["agentActivity"]["signal"] == "stop" => {
            if !app.linear.stop(sid, None) {
                app.linear
                    .surface
                    .finish(&app, sid, Vec::new(), Outcome::Stopped(None))
                    .await;
            }
            return;
        }
        Some("prompted") => {
            let body = p["agentActivity"]["content"]["body"].as_str();
            (body.unwrap_or_default().to_string(), body)
        }
        _ => return,
    };
    tracing::info!("[{sid}] linear {} on {}", p["action"], issue["identifier"]);
    start_turn(&app, sid, issue, prompt, request).await;
}

/// Linear's own `promptContext` (issue, comments, guidance) when present, else a minimal one.
fn initial_prompt(p: &Value) -> String {
    if let Some(ctx) = p["promptContext"].as_str() {
        return ctx.to_string();
    }
    let issue = &p["agentSession"]["issue"];
    let mut prompt = format!(
        "{}: {}\n\n{}",
        issue["identifier"].as_str().unwrap_or_default(),
        issue["title"].as_str().unwrap_or_default(),
        issue["description"].as_str().unwrap_or_default()
    );
    if let Some(comment) = mention_request(p) {
        prompt.push_str("\n\nRequest:\n");
        prompt.push_str(comment);
    }
    prompt
}

/// The @mention that started the session, if it was one. A delegated session also carries a
/// comment, Linear's own thread marker, which is not a request.
fn mention_request(p: &Value) -> Option<&str> {
    p["agentSession"]["comment"]["body"]
        .as_str()
        .filter(|c| !c.is_empty() && !c.contains("This thread is for an agent session"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn mention_is_told_apart_from_delegation() {
        let with = |body: &str| json!({"agentSession": {"comment": {"body": body}}});
        assert_eq!(mention_request(&with("@bot fix it")), Some("@bot fix it"));
        assert_eq!(
            mention_request(&with("This thread is for an agent session with Bot.")),
            None
        );
        assert_eq!(mention_request(&with("")), None);
        assert_eq!(mention_request(&json!({"agentSession": {}})), None);
    }

    #[test]
    fn mention_prompt_includes_request() {
        let p = json!({"agentSession":{"issue":{"identifier":"A-1","title":"T","description":"D"},"comment":{"body":"@bot fix it"}}});
        assert_eq!(initial_prompt(&p), "A-1: T\n\nD\n\nRequest:\n@bot fix it");
        let p = json!({"promptContext":"<issue/>","agentSession":{}});
        assert_eq!(initial_prompt(&p), "<issue/>");
    }
}
