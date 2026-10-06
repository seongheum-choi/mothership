//! Settings, read from the process environment and `<home>/.env` (the environment wins).

use crate::{review::ReviewBackend, store::Tokens, tunnel::Tunnel};
use anyhow::{Context, Result, anyhow};
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
    pub linear: LinearConfig,
    pub zulip: Option<ZulipConfig>,
    /// Main clone that issue worktrees are cut from.
    pub repo: PathBuf,
    pub base_branch: String,
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
    /// Seeds the token store on first start (tokens carried over from an earlier install).
    pub seed_tokens: Option<Tokens>,
}

pub struct ZulipConfig {
    pub site: String,
    pub bot_email: String,
    pub api_key: String,
    pub webhook_token: String,
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
            linear: LinearConfig {
                client_id: vars.require("LINEAR_CLIENT_ID")?,
                client_secret: vars.require("LINEAR_CLIENT_SECRET")?,
                webhook_secret: vars.require("LINEAR_WEBHOOK_SECRET")?,
                seed_tokens,
            },
            zulip,
            repo: PathBuf::from(vars.require("REPO_PATH")?),
            base_branch: vars.get("BASE_BRANCH").unwrap_or_else(|| "main".into()),
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
}
