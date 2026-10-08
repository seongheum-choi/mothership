//! Settings, read from the process environment and `<home>/.env` (the environment wins), and
//! the repository list that is reloaded while running (`reload.rs`).

mod reload;
mod surfaces;
mod vars;

pub use reload::Live;
pub use surfaces::{GitHubConfig, LinearConfig, ZulipConfig};

use crate::{review::ReviewBackend, sandbox, store::Tokens, tunnel::Tunnel};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use vars::{Vars, parse_list, parse_paths};

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
    /// `SANDBOX_READ` and `SANDBOX_WRITE`, opened to every agent's commands.
    pub sandbox: sandbox::Paths,
    pub linear: LinearConfig,
    pub zulip: Option<ZulipConfig>,
    pub github: Option<GitHubConfig>,
    pub worktrees_dir: PathBuf,
    pub claude: ClaudeConfig,
    /// Extra MCP config files for every session, next to the Linear one.
    pub mcp_configs: Vec<PathBuf>,
    /// Replaces the review backend's default instructions in issue sessions.
    pub extra_prompt: Option<String>,
    pub review: ReviewBackend,
    pub tunnel: Option<Tunnel>,
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
        let zulip = ZulipConfig::from_vars(&vars)?;
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
            sandbox: sandbox_paths(&vars, &user_home, &home)?,
            linear: LinearConfig::from_vars(&vars, seed_tokens)?,
            zulip,
            github,
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

    /// Where each turn's `MOTHERSHIP_PROGRESS_FILE` lives.
    pub fn progress_dir(&self) -> PathBuf {
        progress_dir(&self.home)
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

/// `SANDBOX_READ` and `SANDBOX_WRITE`, and the progress directory, since tools append to
/// `MOTHERSHIP_PROGRESS_FILE` from inside Bash's sandbox.
fn sandbox_paths(vars: &Vars, user_home: &Path, home: &Path) -> Result<sandbox::Paths> {
    let read = parse_paths(
        "SANDBOX_READ",
        vars.get("SANDBOX_READ").as_deref(),
        user_home,
    )?;
    let mut write = parse_paths(
        "SANDBOX_WRITE",
        vars.get("SANDBOX_WRITE").as_deref(),
        user_home,
    )?;
    write.push(progress_dir(home));
    Ok(sandbox::Paths { read, write })
}

fn progress_dir(home: &Path) -> PathBuf {
    home.join("progress")
}
