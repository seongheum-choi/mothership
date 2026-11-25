//! Settings, read from the process environment and `<home>/.env` (the environment wins), and
//! the repository list, runner and model defaults that are reloaded while running (`reload.rs`).

mod model;
mod reload;
mod surfaces;
mod vars;

pub use model::{EFFORTS, ModelDefaults, ModelFields, check_effort, efforts};
pub use reload::Live;
pub use surfaces::{
    GerritConfig, GitHubAppConfig, GitHubConfig, LinearConfig, ZulipConfig, trusts,
};
pub use vars::last_value;

use crate::{review::ReviewBackend, sandbox, store::Tokens, tunnel::Tunnel};
use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use vars::{Vars, parse_home_entries, parse_paths};

const DEFAULT_HOST_SANDBOX: &str = if cfg!(target_os = "macos") {
    "/usr/bin/sandbox-exec"
} else {
    "/usr/bin/bwrap"
};

pub struct Config {
    /// State, worktrees, plugins and per-session files live here.
    pub home: PathBuf,
    /// The user's `HOME`, which agents may only read where their work is.
    pub user_home: PathBuf,
    pub bind: String,
    /// Public URL; the Linear OAuth redirect is `<base_url>/callback`.
    pub base_url: String,
    /// Exported to every agent process as `MOTHERSHIP_AGENT`, so hooks and skills can tell
    /// which deployment they run under.
    pub agent_name: String,
    /// Secrets and settings forwarded into every agent process, resolved from the `AGENT_ENV`
    /// key list so values like `CLAUDE_CODE_OAUTH_TOKEN` can live only in `<home>/.env`.
    pub agent_env: Vec<(String, String)>,
    /// `SANDBOX_READ`, `SANDBOX_WRITE` and `SANDBOX_SKIP_DENY`, opened to every agent's commands.
    pub sandbox: sandbox::Paths,
    pub linear: LinearConfig,
    pub zulip: Option<ZulipConfig>,
    pub github: Option<GitHubConfig>,
    /// `GITHUB_APP_CLIENT_ID`: the app whose user token agents get as `GH_TOKEN`.
    pub github_app: Option<GitHubAppConfig>,
    pub gerrit: Option<GerritConfig>,
    pub worktrees_dir: PathBuf,
    pub agent: AgentConfig,
    /// Extra MCP config files for every session, next to the Linear one.
    pub mcp_configs: Vec<PathBuf>,
    /// Replaces the review backend's default instructions in issue sessions.
    pub extra_prompt: Option<String>,
    pub review: ReviewBackend,
    pub tunnel: Option<Tunnel>,
}

/// The CLIs fixed at startup; which one runs a turn and its model settings are reloaded with
/// `.env` (see [`Live`]).
pub struct AgentConfig {
    pub claude_bin: String,
    pub codex_bin: String,
    /// Permission mode for chat sessions; issue sessions always bypass prompts.
    pub chat_permission_mode: String,
    /// `AGENT_START_TIMEOUT`: how long an agent may run without announcing its session
    /// before it is killed.
    pub start_timeout: Duration,
    /// `AGENT_BACKGROUND_TIMEOUT`: how long an agent that answered may keep running for its
    /// background tasks before its input closes.
    pub background_timeout: Duration,
    /// `HOST_SANDBOX`: the `sandbox-exec` (macOS) or `bwrap` (Linux) confining what mothership
    /// runs for a session. `None` when it is not a file at startup: sessions are then not told
    /// about host runs, so their workflows keep that work instead of asking for it.
    pub host_sandbox: Option<String>,
    /// `CONTAINER_RUNTIME`: nerdctl, or another Docker-compatible CLI, starting `sandbox` runs'
    /// containers.
    pub container_runtime: String,
    /// `CONTAINER_START`: a shell command, such as `colima start`, that starts the runtime when
    /// a sandbox run finds it down.
    pub container_start: Option<String>,
    /// `CONTAINER_STOP`: a shell command that stops a runtime `CONTAINER_START` started.
    pub container_stop: Option<String>,
    /// `CONTAINER_IDLE`: how long after the last sandbox run that is.
    pub container_idle: Duration,
}

impl Config {
    pub fn load() -> Result<Self> {
        let user_home = user_home()?;
        let home = home_of(&user_home);
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
        let gerrit = GerritConfig::from_vars(&vars)?;
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
            agent_env: vars.resolve_keys(&vars.list("AGENT_ENV")),
            sandbox: sandbox_paths(&vars, &user_home)?,
            linear: LinearConfig::from_vars(&vars, seed_tokens)?,
            zulip,
            github,
            github_app: GitHubAppConfig::from_vars(&vars),
            gerrit,
            worktrees_dir: vars
                .get("WORKTREES_DIR")
                .map_or_else(|| home.join("worktrees"), PathBuf::from),
            agent: AgentConfig {
                claude_bin: vars.get("CLAUDE_BIN").unwrap_or_else(|| "claude".into()),
                codex_bin: vars.get("CODEX_BIN").unwrap_or_else(|| "codex".into()),
                chat_permission_mode: vars
                    .get("CHAT_PERMISSION_MODE")
                    .unwrap_or_else(|| "auto".into()),
                start_timeout: seconds(&vars, "AGENT_START_TIMEOUT", 120)?,
                background_timeout: seconds(&vars, "AGENT_BACKGROUND_TIMEOUT", 7200)?,
                host_sandbox: Some(
                    vars.get("HOST_SANDBOX")
                        .unwrap_or_else(|| DEFAULT_HOST_SANDBOX.into()),
                )
                .filter(|path| std::path::Path::new(path).is_file()),
                container_runtime: vars
                    .get("CONTAINER_RUNTIME")
                    .unwrap_or_else(|| "nerdctl".into()),
                container_start: vars.get("CONTAINER_START"),
                container_stop: vars.get("CONTAINER_STOP"),
                container_idle: seconds(&vars, "CONTAINER_IDLE", 1800)?,
            },
            mcp_configs: vars.get("MCP_CONFIGS").map_or_default(|list| {
                list.split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(PathBuf::from)
                    .collect()
            }),
            extra_prompt,
            review: vars
                .get("REVIEW_BACKEND")
                .as_deref()
                .unwrap_or("github")
                .parse()?,
            tunnel,
            home,
            user_home,
        })
    }

    /// Where each session's directory (its progress file and host runs) lives.
    pub fn progress_dir(&self) -> PathBuf {
        self.home.join("progress")
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

/// `MOTHERSHIP_HOME`, or `.mothership` in the user's home: where a running instance keeps its
/// state, so the CLI finds the instance there too.
pub fn home() -> Result<PathBuf> {
    Ok(home_of(&user_home()?))
}

fn user_home() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var("HOME").context("HOME is not set")?,
    ))
}

fn home_of(user_home: &Path) -> PathBuf {
    std::env::var("MOTHERSHIP_HOME").map_or_else(|_| user_home.join(".mothership"), PathBuf::from)
}

/// `SANDBOX_READ`, `SANDBOX_WRITE` and `SANDBOX_SKIP_DENY`; each session adds its own directory
/// under the progress directory (see [`crate::app::App::session_dir`]).
fn sandbox_paths(vars: &Vars, user_home: &Path) -> Result<sandbox::Paths> {
    type Parse = fn(&str, Option<&str>, &Path) -> Result<Vec<PathBuf>>;
    let setting = |key: &str, parse: Parse| parse(key, vars.get(key).as_deref(), user_home);
    Ok(sandbox::Paths {
        read: setting("SANDBOX_READ", parse_paths)?,
        write: setting("SANDBOX_WRITE", parse_paths)?,
        skip_deny: setting("SANDBOX_SKIP_DENY", parse_home_entries)?,
    })
}

fn seconds(vars: &Vars, name: &str, default: u64) -> Result<Duration> {
    let Some(value) = vars.get(name) else {
        return Ok(Duration::from_secs(default));
    };
    match value.parse::<u64>() {
        Ok(secs) if secs > 0 => Ok(Duration::from_secs(secs)),
        _ => anyhow::bail!("{name} must be a positive number of seconds, not {value:?}"),
    }
}
