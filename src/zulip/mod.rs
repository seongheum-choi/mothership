//! Zulip conversations through an outgoing-webhook bot: an @mention in a channel or a
//! direct message starts or continues a conversation, one per topic (or DM group).

mod api;
mod conversation;
mod surface;

use crate::{app::App, signature};
use api::{Client, Destination};
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use conversation::{conversation, strip_mention, topic_context};
use serde_json::{Value, json};
use std::{fmt::Write as _, sync::Arc};

const RECEIVED: &str = "eyes";
const ANSWERED: &str = "check";
const CONTEXT_MESSAGES: u32 = 50;

pub struct Zulip;

pub struct Ticket {
    message_id: u64,
    dest: Destination,
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/zulip-webhook", post(webhook))
}

async fn webhook(State(app): State<Arc<App>>, Json(p): Json<Value>) -> (StatusCode, Json<Value>) {
    let ok = (
        StatusCode::OK,
        Json(json!({ "response_not_required": true })),
    );
    let Some(cfg) = &app.cfg.zulip else {
        return (StatusCode::NOT_FOUND, Json(json!({})));
    };
    let token = p["token"].as_str().unwrap_or_default();
    if !signature::constant_time_eq(token.as_bytes(), cfg.webhook_token.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, Json(json!({})));
    }
    let trigger = p["trigger"].as_str().unwrap_or_default();
    if !matches!(trigger, "mention" | "direct_message" | "private_message")
        || p["message"]["id"].as_u64().is_none()
    {
        return ok;
    }
    tokio::spawn(handle(app, p));
    ok
}

async fn handle(app: Arc<App>, p: Value) {
    let Some(zulip) = &app.zulip else { return };
    let message = &p["message"];
    let Some(message_id) = message["id"].as_u64() else {
        return;
    };
    let client = client(&app);
    if let Err(e) = client.react(message_id, RECEIVED, true).await {
        tracing::warn!("zulip reaction failed: {e:#}");
    }
    let (key, dest, location) = conversation(message);
    tracing::info!("[{key}] zulip message {message_id}");

    let mut prompt = String::new();
    if let Destination::Stream { topic, .. } = &dest
        && let Some(channel) = message["display_recipient"].as_str()
    {
        let cursor = app
            .store
            .read(|s| s.sessions.get(&key).and_then(|r| r.cursor));
        match client
            .topic_messages(channel, topic, CONTEXT_MESSAGES)
            .await
        {
            Ok(messages) => prompt.push_str(&topic_context(
                &messages,
                message_id,
                cursor,
                &zulip_bot(&app),
            )),
            Err(e) => tracing::warn!("zulip topic context failed: {e:#}"),
        }
    }
    let _ = write!(
        prompt,
        "From {} ({}):\n{}",
        message["sender_full_name"].as_str().unwrap_or_default(),
        message["sender_email"].as_str().unwrap_or_default(),
        strip_mention(
            p["data"]
                .as_str()
                .or(message["content"].as_str())
                .unwrap_or_default()
        )
    );
    app.store.update(|s| {
        let rec = s.sessions.entry(key.clone()).or_default();
        rec.title = location;
        rec.cursor = Some(message_id);
    });
    zulip.submit(&app, &key, prompt, Ticket { message_id, dest });
}

fn zulip_bot(app: &App) -> String {
    app.cfg
        .zulip
        .as_ref()
        .map(|z| z.bot_email.clone())
        .unwrap_or_default()
}

fn client(app: &App) -> Client<'_> {
    Client {
        http: &app.http,
        cfg: app
            .cfg
            .zulip
            .as_ref()
            .expect("zulip routes run only when configured"),
    }
}
