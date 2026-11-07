//! Runs mothership starts on a session's behalf, for what the agent's own sandbox cannot:
//! Codex, whose login and binary are out of Bash's reach and whose sandbox cannot nest inside
//! Claude Code's (`codex.rs`), and the repository's named commands (`command.rs`). The agent
//! writes a request into its session directory; mothership runs it confined by the session's
//! own rules, as a Seatbelt profile on macOS (`seatbelt.rs`) or bubblewrap mounts on Linux
//! (`bwrap.rs`), and writes the answer next to it (`request.rs`); `process.rs` runs the
//! confined process. The Codex runner's own turns run under the same rules (`codex.rs`).

mod bwrap;
mod codex;
mod command;
mod process;
mod request;
mod seatbelt;

pub use codex::{CodexHome, runner as codex_runner};
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
    /// See [`codex::home_writes`] and [`codex::home_mounts`].
    pub codex_home: CodexHome,
    /// `HOST_SANDBOX`: bubblewrap when its file name is `bwrap`, else `sandbox-exec`.
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

/// Empties [`run_root`] of what earlier runs left and makes it private, at startup, and makes
/// sure sessions' parts of Codex's home ([`codex::state_root`]) have their directory: both
/// must exist for sessions' deny rules to name them.
pub fn reset_run_root(state: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let root = run_root(state);
    if std::fs::symlink_metadata(&root).is_ok() {
        std::fs::remove_dir_all(&root)?;
    }
    let mut dirs = std::fs::DirBuilder::new();
    dirs.recursive(true).mode(0o700);
    dirs.create(&root)?;
    dirs.create(codex::state_root(state))
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

/// `sandbox`'s arguments before the program for a process working in `cwd` under `settings`
/// (see [`seatbelt::profile`] and [`bwrap::args`]), writing also `/dev`, `own` (a run's
/// directory and `TMPDIR`, or a Codex turn's session directory) and, for Codex, Codex's state
/// in its home. Not the shared temporary directories, where one run could reach another's
/// files or plant some for unconfined tools.
pub fn confinement(
    sandbox: &str,
    settings: &Value,
    cwd: &Path,
    own: &Path,
    codex: Option<&CodexHome>,
) -> Vec<String> {
    let mut workspace = vec![cwd.to_path_buf()];
    workspace.extend(crate::sandbox::workspace_clone_git(cwd));
    if Path::new(sandbox).file_name() == Some("bwrap".as_ref()) {
        return bwrap::args(settings, &workspace, own, codex);
    }
    let mut always = seatbelt::subpath(Path::new("/dev"));
    always.extend(
        codex
            .map(|c| codex::home_writes(&c.home))
            .unwrap_or_default(),
    );
    let profile = seatbelt::profile(settings, &workspace, &always, own);
    vec!["-p".into(), profile]
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

    /// [`confinement`] for one of this session's runs.
    fn profile(&self, settings: &Value, run_dir: &Path, codex: Option<&CodexHome>) -> Vec<String> {
        confinement(&self.sandbox, settings, &self.cwd, run_dir, codex)
    }

    fn report(&self, line: &str) {
        if let Err(e) = writeln!(&self.progress, "{line}") {
            tracing::warn!("cannot report {line:?}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A session's tree: a workspace, its directory, a secret its rules deny and Codex's home.
    fn tree() -> PathBuf {
        let base = std::env::temp_dir().join(format!("mothership-cf-{}", std::process::id()));
        for dir in ["ws", "sess", "secret", "codex/sessions"] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        for file in ["secret/s", "codex/auth.json", "codex/config.toml"] {
            std::fs::write(base.join(file), "s").unwrap();
        }
        std::fs::canonicalize(&base).unwrap()
    }

    fn settings(d: &Path) -> Value {
        json!({
            "permissions": {"deny": [format!("Read(/{}/secret/**)", d.display())]},
            "sandbox": {"filesystem": {"allowWrite": [d.join("sess")]}},
        })
    }

    #[test]
    fn confines_a_codex_turn_to_the_sessions_rules() {
        let d = tree();
        let p = |rel: &str| d.join(rel).display().to_string();
        let (ws, own) = (d.join("ws"), d.join("sess"));
        let codex = CodexHome {
            home: d.join("codex"),
            state: d.join("state/s1"),
        };
        let made = |sandbox| confinement(sandbox, &settings(&d), &ws, &own, Some(&codex));

        let bwrap = made("/usr/bin/bwrap");
        for mount in [
            ["--tmpfs", &p("secret")].map(String::from).to_vec(),
            ["--bind", &p("ws"), &p("ws")].map(String::from).to_vec(),
            ["--bind", &p("sess"), &p("sess")]
                .map(String::from)
                .to_vec(),
            ["--tmpfs", &p("codex")].map(String::from).to_vec(),
            ["--bind", &p("state/s1/sessions"), &p("codex/sessions")]
                .map(String::from)
                .to_vec(),
            ["--bind", &p("codex/auth.json"), &p("codex/auth.json")]
                .map(String::from)
                .to_vec(),
            [
                "--ro-bind",
                &p("codex/config.toml"),
                &p("codex/config.toml"),
            ]
            .map(String::from)
            .to_vec(),
        ] {
            assert!(
                bwrap.windows(mount.len()).any(|w| w == mount),
                "{mount:?} in {bwrap:?}"
            );
        }
        assert_eq!(bwrap.last().map(String::as_str), Some("--"));

        let seatbelt = made("/usr/bin/sandbox-exec");
        assert_eq!(seatbelt[0], "-p");
        for rule in [
            format!("(deny file-read* (subpath \"{}\"))", p("secret")),
            format!("(allow file-write* (subpath \"{}\")", p("ws")),
            format!("(literal \"{}\")", p("codex/auth.json")),
            format!("(allow file-read* file-write* (subpath \"{}\"))", p("sess")),
        ] {
            assert!(seatbelt[1].contains(&rule), "{rule} in {}", seatbelt[1]);
        }
        std::fs::remove_dir_all(&d).unwrap();
    }
}
