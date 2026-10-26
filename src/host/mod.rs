//! Runs mothership starts on a session's behalf, for what the agent's own sandbox cannot:
//! Codex, whose login and binary are out of Bash's reach and whose sandbox cannot nest inside
//! Claude Code's (`codex.rs`), and the repository's named commands (`command.rs`). The agent
//! writes a request into its session directory; mothership runs it under a Seatbelt
//! profile made from the session's own rules (`seatbelt.rs`) and writes the answer next to it
//! (`request.rs`); `process.rs` runs the confined process.

mod codex;
mod command;
mod process;
mod request;
mod seatbelt;

pub use command::prompt as commands_prompt;

use crate::app::App;
use request::Request;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

/// How long one run may take before it is killed and answered with an error.
const TIMEOUT: Duration = Duration::from_hours(1);
/// Of a process's own environment, what a run gets besides the agent's.
const INHERITED: [&str; 7] = ["HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "PATH"];

/// What a session's runs need from its turn.
pub struct Host {
    /// The session's directory: requests and answers, next to its progress file. It is what the
    /// session's sandbox may write there, and a sandbox cannot rename or replace the directory
    /// it may write in, only what is inside.
    pub runs: PathBuf,
    /// Where each run gets a directory of its own (see [`process::RunDir`]): one no session
    /// and no other run may read or write.
    pub run_root: PathBuf,
    /// The session's progress file, as the turn opened it, for a run's start and end.
    pub progress: File,
    pub cwd: PathBuf,
    /// The turn's settings, for commands.
    pub settings: Value,
    /// The same with Codex's home and binary readable, for Codex.
    pub codex_settings: Value,
    /// See [`env`].
    pub env: Vec<(String, String)>,
    /// The repository's `commands`, by name.
    pub commands: BTreeMap<String, String>,
    /// `CODEX_BIN`, as found on the agent's `PATH`.
    pub codex: PathBuf,
    /// Codex's home, resolved: see [`codex::home_writes`].
    pub codex_home: PathBuf,
    /// `HOST_SANDBOX`.
    pub sandbox: String,
}

/// Paths a Codex run reads besides the session's: Codex's home (its login and config) and
/// its binaries, found on the agent's `PATH` (see [`codex::binaries`]).
pub fn codex_paths(app: &App) -> Vec<PathBuf> {
    let mut paths = vec![codex_home(app)];
    if let Some(bin) =
        crate::agent::limit::locate(&app.cfg.agent.codex_bin, app.agent_path().as_deref())
    {
        paths.extend(codex::binaries(&bin));
    }
    paths
}

/// Where runs get their own directories: under `MOTHERSHIP_HOME`, so every session's rules
/// deny it.
pub fn run_root(state: &Path) -> PathBuf {
    state.join("host-runs")
}

/// Empties [`run_root`] of what earlier runs left and makes it private, at startup: it must
/// exist for sessions' deny rules to name it.
pub fn reset_run_root(state: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let root = run_root(state);
    if std::fs::symlink_metadata(&root).is_ok() {
        std::fs::remove_dir_all(&root)?;
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)
}

/// `CODEX_HOME` as agents get it, or `~/.codex`.
pub fn codex_home(app: &App) -> PathBuf {
    app.cfg
        .agent_env
        .iter()
        .rev()
        .find(|(key, _)| key == "CODEX_HOME")
        .map_or_else(
            || app.home_dir.join(".codex"),
            |(_, home)| PathBuf::from(home),
        )
}

/// A run's environment: the agent's, less what `settings` hide from Bash (the Claude Code
/// login), over a few basics of mothership's own; nothing else of mothership's.
pub fn env(settings: &Value, agent: &[(String, String)]) -> Vec<(String, String)> {
    let hidden: Vec<&str> = settings["sandbox"]["credentials"]["envVars"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["mode"] == "deny")
        .filter_map(|v| v["name"].as_str())
        .collect();
    INHERITED
        .iter()
        .filter_map(|k| Some(((*k).to_string(), std::env::var(k).ok()?)))
        .chain(agent.iter().cloned())
        .filter(|(k, _)| !hidden.contains(&k.as_str()))
        .collect()
}

/// Serves `host`'s requests one at a time until stopped or dropped; either kills a run in
/// flight.
pub struct Watch(tokio::task::JoinHandle<()>);

impl Watch {
    /// Stops serving and waits until a run in flight is killed, so the workspace is free.
    pub async fn stop(&mut self) {
        self.0.abort();
        let _ = (&mut self.0).await;
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub fn watch(key: String, host: Host) -> Watch {
    Watch(tokio::spawn(async move {
        let mut failed = HashSet::new();
        loop {
            // The agent may replace `runs/` with a link to somewhere mothership should not
            // read requests from or write answers to.
            let real = std::fs::symlink_metadata(&host.runs).is_ok_and(|m| m.is_dir());
            let pending = if real {
                request::pending(&host.runs)
            } else {
                Vec::new()
            };
            for id in pending {
                if failed.contains(&id) {
                    continue;
                }
                let outcome = match request::read_plain(&request::request(&host.runs, &id)) {
                    Ok(text) => match request::parse(&text) {
                        Ok(req) => host.serve(&key, &id, &req).await,
                        Err(e) => Err(format!("refused: {e}")),
                    },
                    Err(e) => Err(format!("unreadable request: {e}")),
                };
                if let Err(e) = request::write_answer(&host.runs, &id, outcome) {
                    tracing::warn!("[{key}] cannot answer run {id}: {e}");
                    failed.insert(id);
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }))
}

impl Host {
    async fn serve(&self, key: &str, id: &str, req: &Request) -> Result<Value, String> {
        let what = match req {
            Request::Codex(_) => "Codex run".to_string(),
            Request::Command(name) => format!("`{name}`"),
        };
        self.report(&format!("{what} {id} started"));
        let outcome = match process::RunDir::create(&self.run_root) {
            Ok(dir) => match req {
                Request::Codex(run) => self.codex(&dir.0, run).await,
                Request::Command(name) => {
                    self.command(&dir.0, name, &request::log(&self.runs, id))
                        .await
                }
            },
            Err(e) => Err(format!("cannot make a directory for the run: {e}")),
        };
        match &outcome {
            Ok(_) => self.report(&format!("{what} {id} finished")),
            Err(e) => {
                tracing::warn!("[{key}] {what} {id} failed: {e}");
                self.report(&format!(
                    "{what} {id} failed: {}",
                    e.lines().next().unwrap_or_default()
                ));
            }
        }
        outcome
    }

    /// The profile for a run under `settings` (see [`seatbelt::profile`]), writing also
    /// `/dev`, `run_dir` (its `TMPDIR` too) and `extra`. Not the shared temporary directories,
    /// where one run could reach another's files or plant some for unconfined tools.
    fn profile(&self, settings: &Value, run_dir: &Path, extra: Vec<String>) -> String {
        let mut workspace = vec![self.cwd.clone()];
        workspace.extend(crate::sandbox::workspace_clone_git(&self.cwd));
        let mut always = seatbelt::subpath(Path::new("/dev"));
        always.extend(extra);
        seatbelt::profile(settings, &workspace, &always, run_dir)
    }

    fn report(&self, line: &str) {
        if let Err(e) = writeln!(&self.progress, "{line}") {
            tracing::warn!("cannot report {line:?}: {e}");
        }
    }
}
