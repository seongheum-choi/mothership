//! What each agent process gets from the instance: the [`Launch`] a surface's [`Place`] becomes,
//! its environment, its settings, and its session's directories.

use super::App;
use crate::{
    agent::Launch,
    sandbox,
    session::choice::{self, Scopes},
    store::SessionRec,
};
use anyhow::Result;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// What a surface decides about its session's next turn; [`App::launch`] adds what every agent
/// gets.
pub struct Place<'a> {
    /// `MOTHERSHIP_SURFACE`: `linear` or `zulip`.
    pub surface: &'a str,
    /// Where the agent works.
    pub cwd: PathBuf,
    pub system_prompt: String,
    /// What the agent may read besides its workspace and the plugin directories.
    pub readable: Vec<PathBuf>,
    /// Permission deny rules beside the sandbox's own.
    pub deny: &'a [String],
    /// MCP configs after the Linear one and `MCP_CONFIGS`.
    pub mcp_configs: Vec<PathBuf>,
    /// The repository's commands the session may ask mothership to run.
    pub commands: BTreeMap<String, String>,
    pub permission_mode: String,
    /// The mode and repository a runner or model may come from.
    pub scopes: Scopes<'a>,
}

/// An agent's `--settings`, and the same rules for what mothership runs on its behalf.
#[derive(Clone)]
pub struct Settings {
    pub agent: Value,
    pub host: Value,
}

impl App {
    /// The turn session `key` (recorded as `rec`) runs next from `place`: its runner and models,
    /// its sandbox settings, MCP configs, plugins and environment.
    pub fn launch(&self, key: &str, rec: &SessionRec, place: Place<'_>) -> Result<Launch> {
        // First: it records what the turn launched, which `settle` needs even when a later step
        // fails the turn.
        let chosen = choice::for_turn(self, key, rec, &place.scopes);
        let plugin_dirs = self.cfg.plugin_dirs();
        let mut readable = place.readable;
        readable.extend(plugin_dirs.iter().cloned());
        let mut mcp_configs = vec![crate::linear::mcp_config(self, key)?];
        mcp_configs.extend(self.cfg.mcp_configs.iter().cloned());
        mcp_configs.extend(place.mcp_configs);
        let settings = self.agent_settings(key, &place.cwd, &readable, place.deny);
        Ok(Launch {
            runner: chosen.runner,
            pinned: chosen.pinned,
            settings: settings.agent,
            host_settings: settings.host,
            commands: place.commands,
            cwd: place.cwd,
            system_prompt: place.system_prompt,
            resume: rec.resume(chosen.runner),
            permission_mode: place.permission_mode,
            models: chosen.models,
            note: chosen.note,
            mcp_configs,
            plugin_dirs,
            env: self.agent_env(place.surface),
        })
    }

    /// Environment every agent process gets: the `MOTHERSHIP_*` markers so hooks and skills know
    /// where they run, plus the `AGENT_ENV` keys resolved from the environment or `<home>/.env`.
    /// With GitHub feedback or Gerrit on, `gh-reply` and `gerrit-reply` come first on `PATH`;
    /// with Gerrit on, its URL and credentials are added (see [`crate::gerrit::agent_env`]).
    pub fn agent_env(&self, surface: &str) -> Vec<(String, String)> {
        let mut env = vec![
            ("MOTHERSHIP_AGENT".into(), self.cfg.agent_name.clone()),
            ("MOTHERSHIP_SURFACE".into(), surface.into()),
        ];
        env.extend(self.cfg.agent_env.iter().cloned());
        if let Some(cfg) = &self.cfg.gerrit {
            crate::gerrit::agent_env(cfg, &mut env, std::env::var("GIT_CONFIG_COUNT").ok());
        }
        let bin_dirs = self.bin_dirs();
        if !bin_dirs.is_empty() {
            let path = env
                .iter()
                .rev()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| v.clone())
                .or_else(|| std::env::var("PATH").ok())
                .unwrap_or_default();
            let mut dirs: Vec<String> = bin_dirs.iter().map(|d| d.display().to_string()).collect();
            dirs.push(path);
            env.push(("PATH".into(), dirs.join(":")));
        }
        env
    }

    /// The directories of mothership's reply wrappers, which agents must be able to read.
    pub fn bin_dirs(&self) -> Vec<PathBuf> {
        let github = self.github.as_ref().map(|g| g.bin_dir.clone());
        let gerrit = self.gerrit.as_ref().map(|g| g.bin_dir.clone());
        github.into_iter().chain(gerrit).collect()
    }

    /// The session's own directory under the progress directory: its progress file and the
    /// runs it asks mothership for. Of that directory, only its own is writable to a session.
    pub fn session_dir(&self, key: &str) -> PathBuf {
        self.cfg.progress_dir().join(crate::store::file_name(key))
    }

    /// The session's own temporary directory (see [`session_tmp`]).
    pub fn session_tmp(&self, key: &str) -> PathBuf {
        session_tmp(&self.cfg.home, key)
    }

    /// The `--settings` for session `key`'s agent working in `workspace`: see
    /// [`sandbox::settings`]. `host` adds what a run mothership starts for the session needs:
    /// Codex's home and binary (see [`crate::host`]).
    pub fn agent_settings(
        &self,
        key: &str,
        workspace: &Path,
        readable: &[PathBuf],
        deny: &[String],
    ) -> Settings {
        let mut paths = self.cfg.sandbox.clone();
        paths.write.push(self.session_dir(key));
        paths.write.push(self.session_tmp(key));
        let settings = |readable: &[PathBuf]| {
            let mut settings = sandbox::settings(
                &self.home_dir,
                &self.cfg.home,
                workspace,
                readable,
                deny,
                &paths,
            );
            sandbox::restrict_network(&mut settings, &self.cfg.home);
            settings
        };
        let mut host_readable = readable.to_vec();
        host_readable.extend(crate::host::codex_paths(self));
        Settings {
            agent: settings(readable),
            host: settings(&host_readable),
        }
    }
}

/// A Unix socket path's limit, its NUL included: `sun_path` in `sockaddr_un`.
const SUN_PATH: usize = if cfg!(target_os = "macos") { 104 } else { 108 };
/// What Claude Code's sandbox adds to `TMPDIR` for its bridge sockets: `/srt-mux-<pid>-<n>.sock`.
const SOCKET_NAME: usize = 30;

/// `TMPDIR` of session `key`: `<state>/session-tmp/` and 16 hex digits of the key's SHA-256.
/// Claude Code's sandbox makes its bridge sockets there, which fail past `sun_path`, so the
/// name has a fixed length however long the key is (a Zulip topic's is the topic). Its own
/// directory under `session-tmp`, not the `tmp` an instance may still share through
/// `SANDBOX_WRITE`, so other sessions' are denied like their progress directories.
pub fn session_tmp(state: &Path, key: &str) -> PathBuf {
    let digest = Sha256::digest(key.as_bytes());
    let head: [u8; 8] = digest[..8].try_into().expect("SHA-256 is 32 bytes");
    state
        .join("session-tmp")
        .join(format!("{:016x}", u64::from_be_bytes(head)))
}

/// Fails when a session's `TMPDIR` under `state` leaves Claude Code's sandbox too little of
/// `sun_path` for its bridge sockets, which would fail every turn's sandbox instead. With
/// `MOTHERSHIP_ALLOW_LONG_TMPDIR` set, as for a `claude` that makes no sockets, startup only
/// warns.
pub fn check_session_tmp(state: &Path) -> Result<()> {
    let tmp = session_tmp(state, "");
    let max = SUN_PATH - 1 - SOCKET_NAME;
    anyhow::ensure!(
        tmp.as_os_str().len() <= max,
        "a session's TMPDIR ({}) is {} bytes, over the {max} that leave room for Claude Code's \
         sandbox sockets (sun_path is {SUN_PATH}); move MOTHERSHIP_HOME to a shorter path",
        tmp.display(),
        tmp.as_os_str().len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_tmp_has_a_fixed_length_whatever_the_key() {
        let state = Path::new("/home/me/.mothership");
        let topic = "a very long Zulip topic about something ".repeat(8);
        let zulip = format!("zulip:engineering-and-operations:{topic}");
        let tmp = session_tmp(state, &zulip);
        let uuid = session_tmp(state, "cc2ccfbf-3742-4ceb-9124-5295b54792d0");
        assert_eq!(tmp.parent(), Some(state.join("session-tmp").as_path()));
        assert_eq!(tmp.as_os_str().len(), state.as_os_str().len() + 29);
        assert_eq!(tmp.as_os_str().len(), uuid.as_os_str().len());
        assert_ne!(tmp, uuid);
        assert!(tmp.as_os_str().len() + SOCKET_NAME < SUN_PATH);
        check_session_tmp(state).unwrap();
    }

    #[test]
    fn refuses_a_home_too_deep_for_the_sandbox_sockets() {
        // `/` and the name, then `/session-tmp/` and 16 digits: one byte over the limit.
        let deep = PathBuf::from("/").join("d".repeat(SUN_PATH - 60));
        let err = check_session_tmp(&deep).unwrap_err().to_string();
        assert!(err.contains("MOTHERSHIP_HOME"), "{err}");
        let fits = PathBuf::from("/").join("d".repeat(SUN_PATH - 61));
        check_session_tmp(&fits).unwrap();
    }
}
