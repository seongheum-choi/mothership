//! Per-surface settings: Linear (always on), Zulip and GitHub (each on when its key is set).

use super::vars::{Vars, parse_list};
use crate::store::Tokens;
use anyhow::{Result, bail};

pub struct LinearConfig {
    pub client_id: String,
    pub client_secret: String,
    pub webhook_secret: String,
    /// `LINEAR_WORKSPACE`: the workspace (URL key or ID) this instance must serve.
    pub workspace: Option<String>,
    /// Seeds the token store on first start (tokens carried over from an earlier install).
    pub seed_tokens: Option<Tokens>,
    /// `LINEAR_API_URL`, the GraphQL and OAuth token host. Left unset except by the integration
    /// tests, which point it at a local mock.
    pub api_url: String,
}

impl LinearConfig {
    pub(super) fn from_vars(vars: &Vars, seed_tokens: Option<Tokens>) -> Result<Self> {
        Ok(Self {
            client_id: vars.require("LINEAR_CLIENT_ID")?,
            client_secret: vars.require("LINEAR_CLIENT_SECRET")?,
            webhook_secret: vars.require("LINEAR_WEBHOOK_SECRET")?,
            workspace: vars.get("LINEAR_WORKSPACE"),
            seed_tokens,
            api_url: linear_api_url(vars.get("LINEAR_API_URL")),
        })
    }
}

pub struct ZulipConfig {
    pub site: String,
    pub bot_email: String,
    pub api_key: String,
    pub webhook_token: String,
}

impl ZulipConfig {
    /// On when `ZULIP_SITE` is set, which then needs the bot's credentials and webhook token.
    pub(super) fn from_vars(vars: &Vars) -> Result<Option<Self>> {
        let Some(site) = vars.get("ZULIP_SITE") else {
            return Ok(None);
        };
        Ok(Some(Self {
            site: site.trim_end_matches('/').to_string(),
            bot_email: vars.require("ZULIP_BOT_EMAIL")?,
            api_key: vars.require("ZULIP_API_KEY")?,
            webhook_token: vars.require("ZULIP_WEBHOOK_TOKEN")?,
        }))
    }
}

pub struct GitHubConfig {
    pub webhook_secret: String,
    /// Logins heard besides the repository owner, such as the owner's own account on an
    /// organisation repository, where GitHub reports it as `MEMBER`.
    pub trusted_logins: Vec<String>,
    /// The agent's own GitHub account; only feedback that mentions it is heard.
    pub mention_login: String,
}

impl GitHubConfig {
    /// On when `GITHUB_WEBHOOK_SECRET` is set, which then needs `GITHUB_MENTION_LOGIN`.
    pub(super) fn from_vars(vars: &Vars) -> Result<Option<Self>> {
        let Some(webhook_secret) = vars.get("GITHUB_WEBHOOK_SECRET") else {
            return Ok(None);
        };
        let mention_login = vars.require("GITHUB_MENTION_LOGIN")?;
        let mention_login = mention_login.trim().trim_start_matches('@').to_string();
        if mention_login.is_empty() {
            bail!("GITHUB_MENTION_LOGIN is empty");
        }
        Ok(Some(Self {
            webhook_secret,
            trusted_logins: vars
                .get("GITHUB_TRUSTED_LOGINS")
                .map(|list| parse_list(&list))
                .unwrap_or_default(),
            mention_login,
        }))
    }
}

/// `LINEAR_API_URL` without trailing slashes, or Linear's own host when it is unset.
fn linear_api_url(setting: Option<String>) -> String {
    setting.map_or_else(
        || "https://api.linear.app".into(),
        |url| url.trim_end_matches('/').to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::linear_api_url;

    #[test]
    fn linear_api_url_defaults_to_linear_and_drops_trailing_slashes() {
        assert_eq!(linear_api_url(None), "https://api.linear.app");
        assert_eq!(
            linear_api_url(Some("http://127.0.0.1:9/".into())),
            "http://127.0.0.1:9"
        );
        assert_eq!(linear_api_url(Some("http://mock//".into())), "http://mock");
        assert_eq!(linear_api_url(Some("http://mock".into())), "http://mock");
    }
}
