//! `/status`: whether the instance is busy or draining, its running turns, and the runner and
//! model defaults in effect, for callers on this machine only.

use super::App;
use crate::session::Activity;
use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
};
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc};

/// Headers a tunnel or proxy adds to every request it forwards: ngrok, Tailscale serve and
/// Funnel, and Cloudflare all set `X-Forwarded-For`; the rest catch proxies set up otherwise.
const FORWARDED: [&str; 5] = [
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "cf-connecting-ip",
    "tailscale-funnel-request",
];

pub(super) async fn status(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !local(peer, &headers) {
        return Err(StatusCode::NOT_FOUND);
    }
    // One look at `.env` and `repos.json` for the whole response; the reads below follow it.
    app.refresh();
    let models = app.models();
    Ok(Json(json!({
        "status": if app.busy() { "busy" } else { "idle" },
        // Also tells `contrib/deploy.sh` that this binary handles SIGUSR1 rather than dying of it.
        "draining": app.draining(),
        "sessions": sessions(&app),
        // Set by `contrib/deploy.sh`, so a deploy can tell the new binary answers.
        "commit": option_env!("MOTHERSHIP_COMMIT"),
        "runner": app.runner(),
        "claude_limited_until": app.claude_limited_until(),
        "model": models.model,
        "fallback_model": models.fallback_model,
        "effort": models.effort,
    })))
}

/// Whether a request comes straight from this machine. A tunnel connects from loopback too, so
/// the peer alone cannot tell; the headers it adds do.
fn local(peer: SocketAddr, headers: &HeaderMap) -> bool {
    peer.ip().to_canonical().is_loopback() && !FORWARDED.iter().any(|h| headers.contains_key(*h))
}

/// Each running turn: where it works, how long it has run, how long its agent has been quiet
/// and the background tasks it still runs. It names no session id and no Zulip workspace, whose
/// name is a topic or the people in a DM, in case a proxy [`local`] does not know reaches it; a
/// Linear one is the issue identifier.
fn sessions(app: &App) -> Vec<Value> {
    let now = crate::store::now_secs();
    let linear = app.linear.activities().into_iter().map(|a| ("linear", a));
    let zulip = app.zulip.iter().flat_map(|z| z.activities()).map(|a| {
        (
            "zulip",
            Activity {
                workspace: None,
                ..a
            },
        )
    });
    linear
        .chain(zulip)
        .map(|(surface, a)| {
            json!({
                "surface": surface,
                "workspace": a.workspace,
                "turn_secs": now.saturating_sub(a.started),
                "quiet_secs": now.saturating_sub(a.last_event),
                "background_tasks": a.background,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(name: &'static str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(name, "203.0.113.7".parse().unwrap());
        map
    }

    #[test]
    fn answers_loopback_without_proxy_headers() {
        let none = HeaderMap::new();
        assert!(local("127.0.0.1:5000".parse().unwrap(), &none));
        assert!(local("[::1]:5000".parse().unwrap(), &none));
        assert!(local("[::ffff:127.0.0.1]:5000".parse().unwrap(), &none));
    }

    #[test]
    fn refuses_other_hosts_and_tunnels() {
        let loopback = "127.0.0.1:5000".parse().unwrap();
        assert!(!local(
            "192.168.1.10:5000".parse().unwrap(),
            &HeaderMap::new()
        ));
        for name in FORWARDED {
            assert!(!local(loopback, &with(name)), "{name}");
        }
    }
}
