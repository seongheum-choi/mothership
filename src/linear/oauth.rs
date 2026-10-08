//! Installing the app: the OAuth authorization redirect and the callback that stores the
//! tokens once their workspace is the one this instance serves.

use super::{Linear, api};
use crate::{
    app::App,
    store::{Tokens, random_hex},
};
use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Redirect,
};
use std::{collections::HashMap, sync::Arc};

/// Only installs into an empty token store: the endpoint is public, so once installed nobody
/// can swap in their own workspace. To install again, clear `linear` in state.json and restart.
pub(super) async fn authorize(
    State(app): State<Arc<App>>,
) -> Result<Redirect, (StatusCode, &'static str)> {
    if app.store.read(|s| !s.linear.access_token.is_empty()) {
        return Err((StatusCode::FORBIDDEN, "already installed"));
    }
    let state = random_hex(16);
    *app.linear
        .surface
        .oauth_state
        .lock()
        .expect("oauth lock poisoned") = Some(state.clone());
    let redirect_uri = format!("{}/callback", app.cfg.base_url);
    let url = reqwest::Url::parse_with_params(
        "https://linear.app/oauth/authorize",
        [
            ("client_id", app.cfg.linear.client_id.as_str()),
            ("redirect_uri", &redirect_uri),
            ("response_type", "code"),
            ("scope", "read,write,app:assignable,app:mentionable"),
            ("actor", "app"),
            ("state", &state),
        ],
    )
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "bad authorize URL"))?;
    Ok(Redirect::to(url.as_str()))
}

pub(super) async fn callback(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> (StatusCode, String) {
    let linear = &app.linear.surface;
    let expected = linear
        .oauth_state
        .lock()
        .expect("oauth lock poisoned")
        .take();
    if expected.is_none() || q.get("state") != expected.as_ref() {
        return (
            StatusCode::BAD_REQUEST,
            "state mismatch; start again at /oauth/authorize".into(),
        );
    }
    let Some(code) = q.get("code") else {
        return (StatusCode::BAD_REQUEST, "missing code".into());
    };
    let tokens = match linear.exchange_code(&app, code).await {
        Ok(tokens) => tokens,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("token exchange failed: {e:#}"),
            );
        }
    };
    let ws = match api::workspace_of(&app, &tokens.access_token).await {
        Ok(ws) => ws,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("workspace lookup failed: {e:#}"),
            );
        }
    };
    // The token is stored only once its workspace is the one this instance serves.
    if let Err(e) = linear.pin(&app, ws) {
        // The details name workspaces, so they stay in the log, not the public response.
        tracing::warn!("refused Linear install: {e:#}");
        return (
            StatusCode::FORBIDDEN,
            "this instance serves another Linear workspace".into(),
        );
    }
    app.store.update(|s| s.linear = tokens);
    (
        StatusCode::OK,
        "Linear authorized. You can close this tab.".into(),
    )
}

impl Linear {
    async fn exchange_code(&self, app: &App, code: &str) -> Result<Tokens> {
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
}
