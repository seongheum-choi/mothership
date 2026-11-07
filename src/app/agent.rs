//! What each agent process gets from the instance: its environment, its settings, and its
//! session's directory.

use super::App;
use crate::sandbox;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// An agent's `--settings`, and the same rules for what mothership runs on its behalf.
#[derive(Clone)]
pub struct Settings {
    pub agent: Value,
    pub host: Value,
}

impl App {
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
