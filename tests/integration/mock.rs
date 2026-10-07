//! Local stand-ins for Linear's GraphQL and OAuth token endpoints and Zulip's REST API, on one
//! port. They answer the calls mothership makes and record what it sent.

use anyhow::{Context, Result};
use axum::{
    Form, Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// The workspace every token belongs to.
pub const ORG: &str = "org-1";
pub const ORG_URL_KEY: &str = "acme";
/// A token Linear rejects, so mothership has to refresh it.
pub const EXPIRED_TOKEN: &str = "expired";
pub const FRESH_TOKEN: &str = "fresh";

#[derive(Default)]
pub struct Recorded {
    /// Issues by id, as `issue(id:)` queries return them.
    pub issues: HashMap<String, Value>,
    /// `agentActivityCreate` inputs.
    pub activities: Vec<Value>,
    /// `issueUpdate` variables.
    pub issue_updates: Vec<Value>,
    /// `/oauth/token` forms.
    pub token_grants: Vec<HashMap<String, String>>,
    /// Zulip `POST /messages` forms.
    pub zulip_messages: Vec<HashMap<String, String>>,
    /// Zulip reactions in order: (`add` or `remove`, message id, emoji).
    pub reactions: Vec<(String, u64, String)>,
}

type Shared = Arc<Mutex<Recorded>>;

pub struct Mock {
    pub url: String,
    recorded: Shared,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Mock {
    pub async fn start() -> Result<Self> {
        let recorded = Shared::default();
        let router = Router::new()
            .route("/graphql", post(graphql))
            .route("/oauth/token", post(token))
            .route("/api/v1/messages", post(zulip_message).get(zulip_history))
            .route(
                "/api/v1/messages/{id}/reactions",
                post(zulip_react).delete(zulip_unreact),
            )
            .with_state(recorded.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("binding the mock server")?;
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
        f(&self.recorded.lock().expect("mock state poisoned"))
    }

    pub fn add_issue(&self, issue: &Value) {
        let id = issue["id"].as_str().expect("test issues have an id");
        self.recorded
            .lock()
            .expect("mock state poisoned")
            .issues
            .insert(id.to_string(), issue.clone());
    }
}

async fn graphql(
    State(recorded): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if token.is_none_or(|t| t == EXPIRED_TOKEN) {
        let errors = json!({"errors": [{"extensions": {"code": "AUTHENTICATION_ERROR"}}]});
        return (StatusCode::UNAUTHORIZED, Json(errors)).into_response();
    }
    let query = body["query"].as_str().unwrap_or_default();
    let variables = &body["variables"];
    let mut recorded = recorded.lock().expect("mock state poisoned");
    let data = if query.contains("viewer") {
        json!({"viewer": {"organization": {"id": ORG, "urlKey": ORG_URL_KEY, "name": "Acme"}}})
    } else if query.contains("agentActivityCreate") {
        recorded.activities.push(variables["input"].clone());
        json!({"agentActivityCreate": {"success": true}})
    } else if query.contains("issueUpdate") {
        recorded.issue_updates.push(variables.clone());
        json!({"issueUpdate": {"success": true}})
    } else if query.contains("issue(") {
        let id = variables["id"].as_str().unwrap_or_default();
        json!({"issue": recorded.issues.get(id)})
    } else {
        let errors = json!({"errors": [{"message": format!("mock: unknown query {query}")}]});
        return (StatusCode::BAD_REQUEST, Json(errors)).into_response();
    };
    Json(json!({ "data": data })).into_response()
}

async fn token(
    State(recorded): State<Shared>,
    Form(form): Form<HashMap<String, String>>,
) -> Json<Value> {
    recorded
        .lock()
        .expect("mock state poisoned")
        .token_grants
        .push(form);
    Json(json!({"access_token": FRESH_TOKEN, "refresh_token": "refresh-2"}))
}

async fn zulip_message(
    State(recorded): State<Shared>,
    Form(form): Form<HashMap<String, String>>,
) -> Json<Value> {
    recorded
        .lock()
        .expect("mock state poisoned")
        .zulip_messages
        .push(form);
    Json(json!({"result": "success", "msg": "", "id": 1000}))
}

async fn zulip_history() -> Json<Value> {
    Json(json!({"result": "success", "msg": "", "messages": []}))
}

async fn zulip_react(
    State(recorded): State<Shared>,
    Path(id): Path<u64>,
    Form(form): Form<HashMap<String, String>>,
) -> Json<Value> {
    react(&recorded, "add", id, &form)
}

async fn zulip_unreact(
    State(recorded): State<Shared>,
    Path(id): Path<u64>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    react(&recorded, "remove", id, &query)
}

fn react(recorded: &Shared, how: &str, id: u64, params: &HashMap<String, String>) -> Json<Value> {
    let emoji = params.get("emoji_name").cloned().unwrap_or_default();
    recorded
        .lock()
        .expect("mock state poisoned")
        .reactions
        .push((how.to_string(), id, emoji));
    Json(json!({"result": "success", "msg": ""}))
}
