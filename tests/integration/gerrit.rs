//! A local stand-in for Gerrit's REST API: it answers the calls mothership and its wrappers make
//! and records what they sent. Like Gerrit, it prefixes JSON with `)]}'` and wants the agent
//! account's HTTP credentials under `/a/`.

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// The agent's Gerrit account.
pub const USERNAME: &str = "acme-bot";
pub const HTTP_PASSWORD: &str = "http-pass";
/// `Basic` + base64(`acme-bot:http-pass`).
const AUTHORIZATION: &str = "Basic aW1wYWxhOmh0dHAtcGFzcw==";

#[derive(Default)]
pub struct Recorded {
    /// What `GET /a/changes/{change}/messages` and `.../comments` answer, by change number.
    pub messages: HashMap<String, Value>,
    pub comments: HashMap<String, Value>,
    /// `POST /a/changes/{change}/revisions/{revision}/review`: change, revision and body.
    pub reviews: Vec<(String, String, Value)>,
}

type Shared = Arc<Mutex<Recorded>>;

pub struct GerritMock {
    pub url: String,
    recorded: Shared,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for GerritMock {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl GerritMock {
    pub async fn start() -> Result<Self> {
        let recorded = Shared::default();
        let router = Router::new()
            .route(
                "/a/changes/{change}/revisions/{revision}/review",
                post(review),
            )
            .route("/a/changes/{change}/messages", get(messages))
            .route("/a/changes/{change}/comments", get(comments))
            .with_state(recorded.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("binding the Gerrit mock")?;
        let url = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(Self {
            url,
            recorded,
            server,
        })
    }

    pub fn read<T>(&self, f: impl FnOnce(&Recorded) -> T) -> T {
        f(&self.recorded.lock().expect("gerrit mock poisoned"))
    }

    /// Sets what change `number`'s messages and comments are.
    pub fn set_review(&self, number: u64, messages: Value, comments: Value) {
        let mut recorded = self.recorded.lock().expect("gerrit mock poisoned");
        recorded.messages.insert(number.to_string(), messages);
        recorded.comments.insert(number.to_string(), comments);
    }
}

fn authorized(headers: &HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(AUTHORIZATION)
}

/// A JSON body the way Gerrit sends it.
fn gerrit_json(value: &Value) -> (StatusCode, String) {
    (StatusCode::OK, format!(")]}}'\n{value}"))
}

async fn review(
    State(recorded): State<Shared>,
    Path((change, revision)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, String) {
    if !authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized".into());
    }
    recorded
        .lock()
        .expect("gerrit mock poisoned")
        .reviews
        .push((change, revision, body));
    gerrit_json(&json!({}))
}

async fn messages(
    State(recorded): State<Shared>,
    Path(change): Path<String>,
    headers: HeaderMap,
) -> (StatusCode, String) {
    listed(&recorded, &headers, |r| r.messages.get(&change).cloned())
}

async fn comments(
    State(recorded): State<Shared>,
    Path(change): Path<String>,
    headers: HeaderMap,
) -> (StatusCode, String) {
    listed(&recorded, &headers, |r| r.comments.get(&change).cloned())
}

fn listed(
    recorded: &Shared,
    headers: &HeaderMap,
    pick: impl FnOnce(&Recorded) -> Option<Value>,
) -> (StatusCode, String) {
    if !authorized(headers) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized".into());
    }
    match pick(&recorded.lock().expect("gerrit mock poisoned")) {
        Some(value) => gerrit_json(&value),
        None => (StatusCode::NOT_FOUND, "Not found".into()),
    }
}
