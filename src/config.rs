//! Settings, read from the process environment and `<home>/.env` (the environment wins).

use crate::{repos::Repo, review::ReviewBackend, store::Tokens, tunnel::Tunnel};
use anyhow::{Context, Result, anyhow, bail};
use std::{collections::HashMap, path::PathBuf};

pub struct Config {
    /// State, worktrees, plugins and per-session files live here.
    pub home: PathBuf,
    pub bind: String,
    /// Public URL; the Linear OAuth redirect is `<base_url>/callback`.
    pub base_url: String,
    /// Exported to every agent process as `MOTHERSHIP_AGENT`, so hooks and skills can tell
    /// which deployment they run under.
    pub agent_name: String,
    /// Secrets and settings forwarded into every agent process, resolved from the `AGENT_ENV`
    /// key list so values like `CLAUDE_CODE_OAUTH_TOKEN` can live only in `<home>/.env`.
    pub agent_env: Vec<(String, String)>,
    pub linear: LinearConfig,
    pub zulip: Option<ZulipConfig>,
    pub github: Option<GitHubConfig>,
    /// From `<home>/repos.json`, or the one `REPO_PATH`/`BASE_BRANCH` describe. Never empty.
    pub repos: Vec<Repo>,
    pub worktrees_dir: PathBuf,
    pub claude: ClaudeConfig,
    /// Extra MCP config files for every session, next to the Linear one.
    pub mcp_configs: Vec<PathBuf>,
    /// Replaces the review backend's default instructions in issue sessions.
    pub extra_prompt: Option<String>,
    pub review: ReviewBackend,
    pub tunnel: Option<Tunnel>,
}

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

pub struct ZulipConfig {
    pub site: String,
    pub bot_email: String,
    pub api_key: String,
    pub webhook_token: String,
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
    fn from_vars(vars: &Vars) -> Result<Option<Self>> {
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

    pub fn trusts(&self, login: &str) -> bool {
        self.trusted_logins
            .iter()
            .any(|l| l.eq_ignore_ascii_case(login))
    }
}

pub struct ClaudeConfig {
    pub bin: String,
    pub model: String,
    pub fallback_model: String,
    /// Permission mode for chat sessions; issue sessions always bypass prompts.
    pub chat_permission_mode: String,
}

impl Config {
    pub fn load() -> Result<Self> {
        let user_home = PathBuf::from(std::env::var("HOME").context("HOME is not set")?);
        let home = std::env::var("MOTHERSHIP_HOME")
            .map_or_else(|_| user_home.join(".mothership"), PathBuf::from);
        let vars = Vars::load(&home);
        let repos = match crate::repos::load(&home.join("repos.json"), &user_home)? {
            Some(repos) => repos,
            None => vec![Repo::single(
                PathBuf::from(vars.require("REPO_PATH")?),
                vars.get("BASE_BRANCH").unwrap_or_else(|| "main".into()),
            )],
        };

        let extra_prompt = vars
            .get("APPEND_SYSTEM_PROMPT_FILE")
            .map(|path| {
                std::fs::read_to_string(&path)
                    .with_context(|| format!("APPEND_SYSTEM_PROMPT_FILE {path}"))
            })
            .transpose()?;
        let zulip = match vars.get("ZULIP_SITE") {
            Some(site) => Some(ZulipConfig {
                site: site.trim_end_matches('/').to_string(),
                bot_email: vars.require("ZULIP_BOT_EMAIL")?,
                api_key: vars.require("ZULIP_API_KEY")?,
                webhook_token: vars.require("ZULIP_WEBHOOK_TOKEN")?,
            }),
            None => None,
        };
        let github = GitHubConfig::from_vars(&vars)?;
        let tunnel = vars
            .get("CLOUDFLARE_TOKEN")
            .map(|token| Tunnel::Cloudflare {
                bin: vars
                    .get("CLOUDFLARED_BIN")
                    .unwrap_or_else(|| "cloudflared".into()),
                token,
            });
        let seed_tokens = vars.get("LINEAR_ACCESS_TOKEN").map(|access_token| Tokens {
            access_token,
            refresh_token: vars.get("LINEAR_REFRESH_TOKEN").unwrap_or_default(),
        });

        Ok(Self {
            bind: vars.get("BIND").unwrap_or_else(|| "127.0.0.1:3456".into()),
            base_url: vars.require("BASE_URL")?.trim_end_matches('/').to_string(),
            agent_name: vars
                .get("AGENT_NAME")
                .unwrap_or_else(|| "mothership".into()),
            agent_env: vars.resolve_keys(
                &vars
                    .get("AGENT_ENV")
                    .map(|list| parse_list(&list))
                    .unwrap_or_default(),
            ),
            linear: LinearConfig {
                client_id: vars.require("LINEAR_CLIENT_ID")?,
                client_secret: vars.require("LINEAR_CLIENT_SECRET")?,
                webhook_secret: vars.require("LINEAR_WEBHOOK_SECRET")?,
                workspace: vars.get("LINEAR_WORKSPACE"),
                seed_tokens,
                api_url: linear_api_url(vars.get("LINEAR_API_URL")),
            },
            zulip,
            github,
            repos,
            worktrees_dir: vars
                .get("WORKTREES_DIR")
                .map_or_else(|| home.join("worktrees"), PathBuf::from),
            claude: ClaudeConfig {
                bin: vars.get("CLAUDE_BIN").unwrap_or_else(|| "claude".into()),
                model: vars.get("CLAUDE_MODEL").unwrap_or_else(|| "opus".into()),
                fallback_model: vars
                    .get("CLAUDE_FALLBACK_MODEL")
                    .unwrap_or_else(|| "sonnet".into()),
                chat_permission_mode: vars
                    .get("CHAT_PERMISSION_MODE")
                    .unwrap_or_else(|| "auto".into()),
            },
            mcp_configs: vars
                .get("MCP_CONFIGS")
                .map(|list| {
                    list.split(',')
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(PathBuf::from)
                        .collect()
                })
                .unwrap_or_default(),
            extra_prompt,
            review: vars
                .get("REVIEW_BACKEND")
                .as_deref()
                .unwrap_or("github")
                .parse()?,
            tunnel,
            home,
        })
    }

    pub fn repo(&self, name: &str) -> Option<&Repo> {
        crate::repos::by_name(&self.repos, name)
    }

    /// Claude Code plugins installed by dropping a directory into `<home>/plugins`.
    /// Read per turn, so adding or removing one needs no restart.
    pub fn plugin_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(self.home.join("plugins"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        dirs
    }
}

struct Vars {
    file: HashMap<String, String>,
    file_path: PathBuf,
}

impl Vars {
    fn load(home: &std::path::Path) -> Self {
        let file_path = home.join(".env");
        let file = std::fs::read_to_string(&file_path)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
            .collect();
        Self { file, file_path }
    }

    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key)
            .ok()
            .or_else(|| self.file.get(key).cloned())
            .filter(|v| !v.is_empty())
    }

    fn require(&self, key: &str) -> Result<String> {
        self.get(key).ok_or_else(|| {
            anyhow!(
                "missing required setting {key} (environment or {})",
                self.file_path.display()
            )
        })
    }

    /// Pair each key with its value, warning about any key that has none so a missing secret
    /// is visible at startup rather than as a silent agent failure.
    fn resolve_keys(&self, keys: &[String]) -> Vec<(String, String)> {
        keys.iter()
            .filter_map(|key| {
                if let Some(value) = self.get(key) {
                    return Some((key.clone(), value));
                }
                tracing::warn!(
                    "AGENT_ENV lists {key}, but it has no value in the environment or {}",
                    self.file_path.display()
                );
                None
            })
            .collect()
    }
}

/// `LINEAR_API_URL` without trailing slashes, or Linear's own host when it is unset.
fn linear_api_url(setting: Option<String>) -> String {
    setting.map_or_else(
        || "https://api.linear.app".into(),
        |url| url.trim_end_matches('/').to_string(),
    )
}

/// Split a comma-separated list (`AGENT_ENV`, `GITHUB_TRUSTED_LOGINS`): trimmed, empties
/// dropped, first occurrence of each kept.
fn parse_list(list: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for key in list.split(',').map(str::trim).filter(|k| !k.is_empty()) {
        if !keys.iter().any(|seen| seen == key) {
            keys.push(key.to_string());
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::{linear_api_url, parse_list};

    #[test]
    fn parses_trimmed_nonempty_keys() {
        assert_eq!(
            parse_list("CLAUDE_CODE_OAUTH_TOKEN, CLAUDE_CODE_EFFORT_LEVEL"),
            ["CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_EFFORT_LEVEL"]
        );
    }

    #[test]
    fn drops_empty_entries() {
        assert_eq!(parse_list(" , A ,, B , "), ["A", "B"]);
    }

    #[test]
    fn deduplicates_keeping_first() {
        assert_eq!(parse_list("A,B,A"), ["A", "B"]);
    }

    #[test]
    fn empty_list_yields_no_keys() {
        let none: [String; 0] = [];
        assert_eq!(parse_list(""), none);
        assert_eq!(parse_list("   "), none);
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
