//! Logging in to the GitHub App with the device flow: at startup, with no user token stored,
//! the log says where to enter a code, and the token arrives once the operator has.

use super::user_token::{Answer, OAUTH_URL, grant};
use crate::{app::App, store::now_secs};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};

#[derive(Deserialize)]
struct Authorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

/// Starts the device flow when the app is set up and no login is stored. A code that expires
/// unused is not renewed; the next start asks again.
pub fn start(app: &Arc<App>) {
    if app.github_user.is_none() || app.store.read(|s| s.github_user.is_some()) {
        return;
    }
    let app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = login(&app).await {
            tracing::error!(
                "GitHub App login failed: {e:#}; agents use AGENT_ENV's GH_TOKEN until a restart \
                 logs in"
            );
        }
    });
}

async fn login(app: &App) -> Result<()> {
    let cfg = &app.github_user.as_ref().expect("checked by start").cfg;
    let res = app
        .http
        .post(format!("{OAUTH_URL}/login/device/code"))
        .header("Accept", "application/json")
        .form(&[("client_id", cfg.client_id.as_str())])
        .send()
        .await
        .context("GitHub device code endpoint")?;
    let status = res.status();
    if !status.is_success() {
        bail!("GitHub device code endpoint: HTTP {status}");
    }
    let code: Authorization = res.json().await.context("GitHub device code answer")?;
    tracing::warn!(
        "GitHub App login: open {} and enter {} within {} minutes, as the account agents act as",
        code.verification_uri,
        code.user_code,
        code.expires_in / 60
    );
    let deadline = now_secs().saturating_add(code.expires_in);
    let mut interval = code.interval.max(1);
    let form = [
        ("device_code", code.device_code.as_str()),
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
    ];
    while now_secs() < deadline {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        match grant(app, cfg, &form).await?.answer(now_secs()) {
            Answer::Tokens(tokens) => {
                app.store.update(|s| s.github_user = Some(tokens));
                tracing::info!("GitHub App login done; agents get its user token as GH_TOKEN");
                return Ok(());
            }
            Answer::Pending => {}
            // GitHub asks for 5 more seconds each time, and says the new interval.
            Answer::SlowDown(next) => interval = next.unwrap_or(interval + 5),
            Answer::Refused(error, description) => bail!("{error}: {description}"),
        }
    }
    bail!("the code expired before it was entered")
}
