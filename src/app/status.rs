//! `/status`: whether the instance is busy or draining, its running turns, and the runner and
//! model defaults in effect.

use super::App;
use crate::session::Activity;
use axum::{Json, extract::State};
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) async fn status(State(app): State<Arc<App>>) -> Json<Value> {
    // One look at `.env` and `repos.json` for the whole response; the reads below follow it.
    app.refresh();
    let models = app.models();
    Json(json!({
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
    }))
}

/// Each running turn: where it works, how long it has run, how long its agent has been quiet
/// and the background tasks it still runs. A tunnel exposes `/status`, so it names no session
/// id and no Zulip workspace, whose name is a topic or the people in a DM; a Linear one is the
/// issue identifier.
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
