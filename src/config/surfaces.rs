//! Per-surface settings: Linear (always on), Zulip, GitHub and Gerrit (each on when its key is
//! set).

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
    /// `LINEAR_TRUSTED_USERS`: Linear user ids or emails who may call the agent; empty, anyone.
    pub trusted_users: Vec<String>,
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
            trusted_users: list(vars, "LINEAR_TRUSTED_USERS"),
        })
    }
}

pub struct ZulipConfig {
    pub site: String,
    pub bot_email: String,
    pub api_key: String,
    pub webhook_token: String,
    /// `ZULIP_TRUSTED_EMAILS`: sender emails the bot answers; empty, anyone.
    pub trusted_emails: Vec<String>,
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
            trusted_emails: list(vars, "ZULIP_TRUSTED_EMAILS"),
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
            trusted_logins: list(vars, "GITHUB_TRUSTED_LOGINS"),
            mention_login,
        }))
    }
}

/// The GitHub App agents log in to with the device flow, so their `GH_TOKEN` acts as the user
/// within the app's installations and permissions.
#[derive(Clone)]
pub struct GitHubAppConfig {
    pub client_id: String,
    /// Sent with refresh grants when set; GitHub's documentation lists it as required there.
    pub client_secret: Option<String>,
}

impl GitHubAppConfig {
    /// On when `GITHUB_APP_CLIENT_ID` is set.
    pub(super) fn from_vars(vars: &Vars) -> Option<Self> {
        Some(Self {
            client_id: vars.get("GITHUB_APP_CLIENT_ID")?,
            client_secret: vars.get("GITHUB_APP_CLIENT_SECRET"),
        })
    }
}

pub struct GerritConfig {
    /// `GERRIT_URL` without trailing slashes, such as `https://gerrit.example.com`.
    pub url: String,
    /// The agent's Gerrit account and its HTTP password, for mothership's REST calls and handed
    /// to agents for `git push` and `gerrit-reply`.
    pub username: String,
    pub http_password: String,
    /// `GERRIT_WEBHOOK_TOKEN`: the bearer token `POST /gerrit-webhook` takes; unset, review
    /// feedback is off.
    pub webhook_token: Option<String>,
    /// Account emails whose reviews are heard.
    pub trusted_emails: Vec<String>,
    /// What a review writes after `@` to address the agent: `GERRIT_MENTION`, or the username.
    pub mention: String,
}

impl GerritConfig {
    /// On when `GERRIT_URL` is set, which then needs the account's HTTP credentials. Review
    /// feedback is on when `GERRIT_WEBHOOK_TOKEN` is set, which then needs
    /// `GERRIT_TRUSTED_EMAILS`.
    pub(super) fn from_vars(vars: &Vars) -> Result<Option<Self>> {
        let Some(url) = vars.get("GERRIT_URL") else {
            return Ok(None);
        };
        let username = vars.require("GERRIT_USERNAME")?;
        let webhook_token = vars.get("GERRIT_WEBHOOK_TOKEN");
        let trusted_emails = list(vars, "GERRIT_TRUSTED_EMAILS");
        if webhook_token.is_some() && trusted_emails.is_empty() {
            bail!("GERRIT_WEBHOOK_TOKEN is set but GERRIT_TRUSTED_EMAILS lists nobody");
        }
        let mention = vars
            .get("GERRIT_MENTION")
            .map(|m| m.trim().trim_start_matches('@').to_string())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| username.clone());
        Ok(Some(Self {
            url: url.trim_end_matches('/').to_string(),
            username,
            http_password: vars.require("GERRIT_HTTP_PASSWORD")?,
            webhook_token,
            trusted_emails,
            mention,
        }))
    }
}

/// The comma-separated list in `key`, empty when unset.
fn list(vars: &Vars, key: &str) -> Vec<String> {
    vars.get(key).map(|l| parse_list(&l)).unwrap_or_default()
}

/// Whether someone named by any of `names` (an id, an email) may call the agent: anyone when
/// `trusted` is empty, else only a name it lists, in any letter case. Someone the event does
/// not name is refused.
pub fn trusts(trusted: &[String], names: &[Option<&str>]) -> bool {
    trusted.is_empty()
        || names
            .iter()
            .flatten()
            .any(|n| trusted.iter().any(|t| t.eq_ignore_ascii_case(n)))
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
    use super::{linear_api_url, trusts};

    #[test]
    fn an_empty_list_trusts_anyone_and_a_list_only_its_names() {
        assert!(trusts(&[], &[None]));
        assert!(trusts(&[], &[Some("eve@x")]));
        let list = ["user-1".to_string(), "Alice@Example.com".to_string()];
        assert!(trusts(&list, &[Some("user-1"), None]));
        assert!(trusts(&list, &[Some("user-9"), Some("alice@example.com")]));
        assert!(!trusts(&list, &[Some("user-9"), Some("eve@example.com")]));
        assert!(!trusts(&list, &[None, None]), "unnamed is refused");
        assert!(!trusts(&list, &[Some("")]));
    }

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
