//! The GitHub App user access token agents get as `GH_TOKEN`: acting as the user who logged in,
//! limited to the repositories the app is installed on and the app's permissions (Checks
//! included). Refreshed before a turn when it is close to expiring; without one, agents keep
//! the `GH_TOKEN` of `AGENT_ENV`.

use crate::{app::App, config::GitHubAppConfig, store::now_secs};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// GitHub's OAuth host, for the device flow and the token grants.
pub(super) const OAUTH_URL: &str = "https://github.com";

/// A turn gets a token good for at least this long: refreshed once less than this remains of
/// its 8 hours, so turns with long background work keep a working `gh`.
const REFRESH_MARGIN: u64 = 4 * 3600;

/// What `state.json` keeps of the login. Expiry times are Unix seconds; `None` never expires
/// (an app with token expiration turned off).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UserTokens {
    pub access_token: String,
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub refresh_token: String,
}

impl UserTokens {
    fn needs_refresh(&self, now: u64) -> bool {
        self.expires_at
            .is_some_and(|at| at <= now.saturating_add(REFRESH_MARGIN))
    }
}

/// The app's client and the lock that keeps refreshes one at a time: GitHub rotates the refresh
/// token, so two concurrent refreshes would leave one of them holding a dead token.
pub struct UserToken {
    pub(super) cfg: GitHubAppConfig,
    lock: tokio::sync::Mutex<()>,
}

impl UserToken {
    pub fn new(cfg: GitHubAppConfig) -> Self {
        Self {
            cfg,
            lock: tokio::sync::Mutex::default(),
        }
    }
}

/// A token grant's answer, success and failure alike: GitHub reports errors with HTTP 200.
#[derive(Deserialize, Default)]
pub(super) struct Grant {
    access_token: Option<String>,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
    interval: Option<u64>,
}

#[derive(Debug, PartialEq)]
pub(super) enum Answer {
    Tokens(UserTokens),
    /// Device flow: the user has not entered the code yet.
    Pending,
    /// Device flow: polling too fast; wait this many seconds between polls from now on.
    SlowDown(Option<u64>),
    /// The grant was refused: the error code and GitHub's description of it.
    Refused(String, String),
}

impl Grant {
    pub(super) fn answer(self, now: u64) -> Answer {
        match (self.access_token, self.error) {
            (Some(access_token), None) => Answer::Tokens(UserTokens {
                access_token,
                expires_at: self.expires_in.map(|secs| now.saturating_add(secs)),
                refresh_token: self.refresh_token.unwrap_or_default(),
            }),
            (_, Some(e)) if e == "authorization_pending" => Answer::Pending,
            (_, Some(e)) if e == "slow_down" => Answer::SlowDown(self.interval),
            (_, error) => Answer::Refused(
                error.unwrap_or_else(|| "no_token".into()),
                self.error_description.unwrap_or_default(),
            ),
        }
    }
}

/// Posts a grant to GitHub's token endpoint.
pub(super) async fn grant(
    app: &App,
    cfg: &GitHubAppConfig,
    form: &[(&str, &str)],
) -> Result<Grant> {
    let mut form = form.to_vec();
    form.push(("client_id", &cfg.client_id));
    if let Some(secret) = &cfg.client_secret {
        form.push(("client_secret", secret));
    }
    let res = app
        .http
        .post(format!("{OAUTH_URL}/login/oauth/access_token"))
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .context("GitHub token endpoint")?;
    let status = res.status();
    if !status.is_success() {
        bail!("GitHub token endpoint: HTTP {status}");
    }
    res.json().await.context("GitHub token endpoint answer")
}

/// The token for the next turn, refreshed first when it is close to expiring. `None` (no app,
/// no login yet, or a failed refresh) leaves agents with `AGENT_ENV`'s `GH_TOKEN`.
pub async fn for_turn(app: &App) -> Option<String> {
    let user = app.github_user.as_ref()?;
    let _guard = user.lock.lock().await;
    let tokens = app.store.read(|s| s.github_user.clone())?;
    if !tokens.needs_refresh(now_secs()) {
        return Some(tokens.access_token);
    }
    match refresh(app, &user.cfg, &tokens.refresh_token).await {
        Ok(Answer::Tokens(fresh)) => {
            let token = fresh.access_token.clone();
            app.store.update(|s| s.github_user = Some(fresh));
            tracing::info!("GitHub user token refreshed");
            Some(token)
        }
        Ok(Answer::Refused(error, _)) if error == "bad_refresh_token" => {
            // An expired or revoked refresh token never works again: drop the login so the
            // next start asks for a new one.
            app.store.update(|s| s.github_user = None);
            tracing::error!(
                "GitHub refused the refresh token; agents use AGENT_ENV's GH_TOKEN until a \
                 restart logs in again"
            );
            None
        }
        Ok(answer) => {
            tracing::warn!(
                "GitHub user token refresh refused ({answer:?}); this turn uses AGENT_ENV's GH_TOKEN"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                "GitHub user token refresh failed: {e:#}; this turn uses AGENT_ENV's GH_TOKEN"
            );
            None
        }
    }
}

async fn refresh(app: &App, cfg: &GitHubAppConfig, refresh_token: &str) -> Result<Answer> {
    if refresh_token.is_empty() {
        bail!("no refresh token stored");
    }
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ];
    Ok(grant(app, cfg, &form).await?.answer(now_secs()))
}

/// Puts `token` in `env` as `GH_TOKEN`, in place of the one `AGENT_ENV` forwarded.
pub fn set_gh_token(env: &mut Vec<(String, String)>, token: String) {
    env.retain(|(key, _)| key != "GH_TOKEN");
    env.push(("GH_TOKEN".into(), token));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(json: &str) -> Answer {
        serde_json::from_str::<Grant>(json).unwrap().answer(1000)
    }

    #[test]
    fn reads_tokens_with_and_without_expiry() {
        let answer = grant(
            r#"{"access_token":"ghu_a","expires_in":28800,"refresh_token":"ghr_b",
                "refresh_token_expires_in":15897600,"token_type":"bearer","scope":""}"#,
        );
        assert_eq!(
            answer,
            Answer::Tokens(UserTokens {
                access_token: "ghu_a".into(),
                expires_at: Some(29800),
                refresh_token: "ghr_b".into(),
            })
        );
        let Answer::Tokens(t) = grant(r#"{"access_token":"ghu_a","token_type":"bearer"}"#) else {
            panic!("no tokens");
        };
        assert_eq!((t.expires_at, t.refresh_token.as_str()), (None, ""));
    }

    #[test]
    fn reads_device_flow_errors() {
        assert_eq!(
            grant(r#"{"error":"authorization_pending"}"#),
            Answer::Pending
        );
        assert_eq!(
            grant(r#"{"error":"slow_down","interval":10}"#),
            Answer::SlowDown(Some(10))
        );
        assert_eq!(
            grant(r#"{"error":"bad_refresh_token","error_description":"expired"}"#),
            Answer::Refused("bad_refresh_token".into(), "expired".into())
        );
        assert_eq!(
            grant("{}"),
            Answer::Refused("no_token".into(), String::new())
        );
    }

    #[test]
    fn refreshes_when_less_than_the_margin_remains() {
        let tokens = |expires_at| UserTokens {
            access_token: "a".into(),
            expires_at,
            refresh_token: "r".into(),
        };
        assert!(!tokens(None).needs_refresh(u64::MAX));
        assert!(!tokens(Some(1000 + REFRESH_MARGIN + 1)).needs_refresh(1000));
        assert!(tokens(Some(1000 + REFRESH_MARGIN)).needs_refresh(1000));
        assert!(tokens(Some(500)).needs_refresh(1000));
    }

    #[test]
    fn replaces_the_forwarded_gh_token() {
        let mut env = vec![
            ("GH_TOKEN".into(), "pat".into()),
            ("PATH".into(), "/bin".into()),
        ];
        set_gh_token(&mut env, "ghu".into());
        assert_eq!(
            env,
            [
                ("PATH".into(), "/bin".into()),
                ("GH_TOKEN".into(), "ghu".into())
            ]
        );
    }
}
