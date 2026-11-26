//! The Linear MCP server agents get: mothership itself, proxying Linear's hosted MCP with the
//! token current at each request, so a turn outlives a token refresh.

use crate::app::App;
use anyhow::{Context, Result};
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Method, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, LazyLock},
};

pub const PATH: &str = "/linear/mcp";

/// Request headers MCP's streamable HTTP transport needs upstream; the rest stay local.
const FORWARDED: [&str; 5] = [
    "accept",
    "content-type",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];
/// Response headers the agent's MCP client reads.
const RETURNED: [&str; 2] = ["content-type", "mcp-session-id"];

/// What agents present to the proxy, which can be reached wherever the webhooks can. New on
/// each start, since a restart waits for running turns first.
static SECRET: LazyLock<String> = LazyLock::new(|| crate::store::random_hex(32));

/// The MCP config pointing the agent at the proxy.
pub fn config(app: &App, key: &str) -> Result<PathBuf> {
    let config = json!({ "mcpServers": { "linear": {
        "type": "http",
        "url": format!("http://{}{PATH}", local_addr(&app.cfg.bind)),
        "headers": { "Authorization": format!("Bearer {}", *SECRET) },
    }}});
    let dir = app.cfg.home.join("mcp");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", crate::store::file_name(key)));
    crate::store::write_private(&path, &serde_json::to_vec(&config)?)?;
    Ok(path)
}

/// Where a local client reaches `bind`: a wildcard address is reached through loopback.
fn local_addr(bind: &str) -> String {
    match bind.parse::<SocketAddr>() {
        Ok(SocketAddr::V4(a)) if a.ip().is_unspecified() => format!("127.0.0.1:{}", a.port()),
        Ok(SocketAddr::V6(a)) if a.ip().is_unspecified() => format!("[::1]:{}", a.port()),
        _ => bind.to_string(),
    }
}

pub async fn proxy(
    State(app): State<Arc<App>>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let given = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !crate::signature::constant_time_eq(given.as_bytes(), SECRET.as_bytes()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match forward(&app, &method, &headers, &body).await {
        Ok(upstream) => relay(upstream),
        Err(e) => {
            tracing::warn!("Linear MCP proxy: {e:#}");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// Sends the request with the stored token, refreshing it once if Linear rejects it.
async fn forward(
    app: &App,
    method: &Method,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<reqwest::Response> {
    let token = app.store.read(|s| s.linear.access_token.clone());
    let upstream = send(app, method, headers, body, &token).await?;
    if upstream.status() != StatusCode::UNAUTHORIZED {
        return Ok(upstream);
    }
    app.linear.surface.refresh(app, &token).await?;
    let token = app.store.read(|s| s.linear.access_token.clone());
    send(app, method, headers, body, &token).await
}

async fn send(
    app: &App,
    method: &Method,
    headers: &HeaderMap,
    body: &Bytes,
    token: &str,
) -> Result<reqwest::Response> {
    let mut req = app
        .http
        .request(method.clone(), &app.cfg.linear.mcp_url)
        .bearer_auth(token)
        .body(body.clone());
    for name in FORWARDED {
        if let Some(value) = headers.get(name) {
            req = req.header(name, value);
        }
    }
    req.send().await.context("reaching Linear MCP")
}

/// Streams the answer back as it comes, since MCP answers in server-sent events.
fn relay(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let mut headers = HeaderMap::new();
    for name in RETURNED {
        if let Some(value) = upstream.headers().get(name) {
            headers.insert(name, value.clone());
        }
    }
    let chunks = futures_util::stream::unfold(upstream, |mut r| async move {
        r.chunk().await.transpose().map(|chunk| (chunk, r))
    });
    (status, headers, Body::from_stream(chunks)).into_response()
}

#[cfg(test)]
mod tests {
    use super::local_addr;

    #[test]
    fn local_addr_reaches_wildcards_through_loopback() {
        assert_eq!(local_addr("0.0.0.0:3456"), "127.0.0.1:3456");
        assert_eq!(local_addr("[::]:3456"), "[::1]:3456");
        assert_eq!(local_addr("127.0.0.1:3456"), "127.0.0.1:3456");
        assert_eq!(local_addr("10.0.0.2:80"), "10.0.0.2:80");
        assert_eq!(local_addr("localhost:3456"), "localhost:3456");
    }
}
