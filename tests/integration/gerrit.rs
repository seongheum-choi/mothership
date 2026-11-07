//! A local stand-in for Gerrit's REST API: it answers the calls mothership and its wrappers make
//! and records what they sent. Like Gerrit, it prefixes JSON with `)]}'` and wants the agent
//! account's HTTP credentials under `/a/`.

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// The agent's Gerrit account.
pub const USERNAME: &str = "acme-bot";
pub const HTTP_PASSWORD: &str = "http-pass";
/// `Basic` + base64(`acme-bot:http-pass`).
const AUTHORIZATION: &str = "Basic aW1wYWxhOmh0dHAtcGFzcw==";

#[derive(Default)]
pub struct Recorded {
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
