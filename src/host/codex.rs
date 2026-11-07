//! A Codex run, and a Codex runner turn: `codex exec` with Codex's own sandbox off, since the
//! profile is the sandbox.

use super::{Host, request, seatbelt};
use crate::{
    agent::{Confined, Launch},
    app::App,
};
use anyhow::Context;
use request::CodexRun;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The directories a run may write in Codex's home: its sessions, logs and caches. Under
/// bubblewrap, each is the session's own (see [`CodexHome::state`]).
const HOME_DIRS: [&str; 8] = [
    "sessions",
    "thread-writer-locks",
    "archived_sessions",
    "log",
    "cache",
    "shell_snapshots",
    ".tmp",
    "tmp",
];

/// The files a run may write there besides its state databases: a refreshed login, its caches
/// and state.
const HOME_FILES: [&str; 7] = [
    "auth.json",
    "models_cache.json",
    "history.jsonl",
    "installation_id",
    ".codex-global-state.json",
    ".codex-global-state.json.bak",
    ".sqlite-maintenance.lock",
];

/// Of [`HOME_FILES`], what a run under bubblewrap writes through to Codex's home: a login
/// refreshed and not kept would leave the home's login with a spent refresh token.
const SHARED_FILES: [&str; 2] = ["auth.json", "installation_id"];

/// What a run may write in Codex's home, as Seatbelt filters: [`HOME_DIRS`], [`HOME_FILES`]
/// and its state databases. Not its config, hooks, rules, skills, memories or binaries, which
/// later runs, the Codex runner and the user's own Codex would pick up.
pub fn home_writes(home: &Path) -> Vec<String> {
    let mut writes: Vec<String> = HOME_DIRS
        .iter()
        .flat_map(|dir| seatbelt::subpath(&home.join(dir)))
        .collect();
    writes.extend(
        HOME_FILES
            .iter()
            .flat_map(|file| seatbelt::literal(&home.join(file))),
    );
    writes.push(seatbelt::regex(&format!(
        "^{}/[^/]*\\.sqlite(-shm|-wal|-journal)?$",
        seatbelt::escape(&home.to_string_lossy())
    )));
    writes
}

/// Codex's home, and the session's own part of it.
pub struct CodexHome {
    /// Codex's home, resolved.
    pub home: PathBuf,
    /// `<MOTHERSHIP_HOME>/codex/<session>`: the session's [`HOME_DIRS`], which bubblewrap
    /// mounts in Codex's home so one session cannot read or change another's threads. No
    /// session's rules let it read the directory itself.
    pub state: PathBuf,
}

impl CodexHome {
    pub fn of(app: &App, key: &str) -> Self {
        let home = super::codex_home(app);
        Self {
            home: std::fs::canonicalize(&home).unwrap_or(home),
            state: state_root(&app.cfg.home).join(crate::store::file_name(key)),
        }
    }
}

/// Where sessions keep their own parts of Codex's home.
pub fn state_root(state: &Path) -> PathBuf {
    state.join("codex")
}

/// Codex's home as bubblewrap mounts: an empty directory in its place, gone after the run,
/// with the home's entries mounted back in: writable for [`SHARED_FILES`], links as links,
/// the rest read-only (config, hooks, rules, skills, memories). [`HOME_DIRS`] come from the
/// session's own directory instead, made when missing. The state databases and the other
/// [`HOME_FILES`] are left out, so the run starts its own: a database shared with Codex
/// outside the run, its journal in the throwaway directory, would be corrupted. Codex finds
/// a thread to resume from its file in `sessions` alone. Whatever else a run writes there
/// goes with it, so it leaves no config behind.
pub fn home_mounts(codex: &CodexHome) -> Vec<String> {
    let home = &codex.home;
    let Ok(entries) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut mounts = vec!["--tmpfs".to_string(), home.to_string_lossy().into_owned()];
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path().to_string_lossy().into_owned();
        if HOME_DIRS.contains(&name.as_str()) {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_symlink()) {
            if let Ok(to) = std::fs::read_link(entry.path()) {
                mounts.extend(["--symlink".into(), to.to_string_lossy().into_owned(), path]);
            }
            continue;
        }
        let kind = if SHARED_FILES.contains(&name.as_str()) {
            "--bind"
        } else if HOME_FILES.contains(&name.as_str()) || name.contains(".sqlite") {
            continue;
        } else {
            "--ro-bind"
        };
        mounts.extend([kind.into(), path.clone(), path]);
    }
    for dir in HOME_DIRS {
        let own = codex.state.join(dir);
        // bubblewrap binds only what exists.
        if let Err(e) = private_dir(&own) {
            tracing::warn!("cannot make {}: {e}", own.display());
            continue;
        }
        let at = home.join(dir).to_string_lossy().into_owned();
        mounts.extend(["--bind".into(), own.to_string_lossy().into_owned(), at]);
    }
    mounts
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Codex's binary and the code-mode host it starts its tools through, which it looks for
/// beside the path it was started as, each as named and as resolved. Without the host,
/// every tool call fails with "Operation not permitted".
pub fn binaries(bin: &Path) -> Vec<PathBuf> {
    let host = bin.with_file_name("codex-code-mode-host");
    [bin.to_path_buf(), host]
        .into_iter()
        .filter(|p| p.exists())
        .flat_map(|p| [std::fs::canonicalize(&p).ok(), Some(p)])
        .flatten()
        .collect()
}

/// How Codex starts for a turn of session `key`: `HOST_SANDBOX` (and its seccomp filter under
/// bubblewrap) with the turn's host settings
/// (the session's read and write rules, Codex's home and binary readable) and the session's
/// directory as its own, then `CODEX_BIN`, so Codex reads only what Claude Code's sandbox lets
/// the session read. Its environment is a run's (see [`super::env`]), not mothership's, with
/// `TMPDIR` in the session's directory, since `/tmp` is not writable and no other session's.
/// Without `HOST_SANDBOX` there is none, and the turn fails rather than read the whole home.
pub fn runner(app: &App, key: &str, launch: &Launch) -> anyhow::Result<Confined> {
    let sandbox = (app.cfg.agent.host_sandbox.clone())
        .context("HOST_SANDBOX is not a file, so Codex cannot run confined to the session")?;
    let bin = &app.cfg.agent.codex_bin;
    let bin =
        crate::agent::limit::locate(bin, app.agent_path().as_deref()).unwrap_or_else(|| bin.into());
    let own = app.session_dir(key);
    let tmp = own.join("tmp");
    std::fs::create_dir_all(&tmp).with_context(|| format!("making {}", tmp.display()))?;
    let codex = CodexHome::of(app, key);
    let filter = super::run_root(&app.cfg.home).join(super::seccomp::FILE);
    let prefix = super::confinement(
        &sandbox,
        &filter,
        &launch.host_settings,
        &launch.cwd,
        &own,
        Some(&codex),
    )
    .map_err(anyhow::Error::msg)?;
    let mut env = super::env(&launch.settings, &launch.env);
    env.push(("TMPDIR".into(), tmp.display().to_string()));
    Ok(Confined {
        argv: prefix
            .into_iter()
            .chain([bin.display().to_string()])
            .collect(),
        env,
    })
}

impl Host {
    pub(super) async fn codex(&self, dir: &Path, run: &CodexRun) -> Result<Value, String> {
        let last = dir.join("last");
        let mut args: Vec<String> = vec![
            "exec".into(),
            "--skip-git-repo-check".into(),
            // Codex's own sandbox cannot start inside the profile's.
            "--sandbox".into(),
            "danger-full-access".into(),
            "-C".into(),
            self.cwd.display().to_string(),
            "-o".into(),
            last.display().to_string(),
        ];
        if let Some(schema) = &run.schema {
            let file = dir.join("schema.json");
            request::write_new(&file, schema.to_string().as_bytes()).map_err(|e| e.to_string())?;
            args.extend(["--output-schema".into(), file.display().to_string()]);
        }
        if let Some(model) = &run.model {
            args.push(format!("--model={model}"));
        }
        if let Some(effort) = &run.effort {
            args.extend(["-c".into(), format!("model_reasoning_effort=\"{effort}\"")]);
        }
        args.push("-".into());
        let profile = self.profile(&self.codex_settings, dir, Some(&self.codex_home))?;
        let codex = self.codex.display().to_string();
        let out = self
            .confined(profile, dir, &codex, &args, Some(&run.prompt))
            .await?;
        // Codex's commands could have put a link where its final message goes.
        let text = request::read_plain(&last).unwrap_or_default();
        if !out.status.success() || text.trim().is_empty() {
            return Err(format!(
                "codex exited ({}): {}",
                out.status,
                super::process::tail(&out.stderr, 5)
            ));
        }
        match run.schema {
            Some(_) => {
                serde_json::from_str(&text).map_err(|e| format!("final message is not JSON: {e}"))
            }
            None => Ok(Value::String(text.trim().to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_code_mode_host_beside_the_binary() {
        let dir = std::env::temp_dir().join(format!("mothership-codex-{}", std::process::id()));
        let real = dir.join("release");
        std::fs::create_dir_all(&real).unwrap();
        let bin = dir.join("codex");
        std::fs::write(real.join("codex"), "").unwrap();
        std::fs::write(real.join("codex-code-mode-host"), "").unwrap();
        std::os::unix::fs::symlink(real.join("codex"), &bin).unwrap();
        let host = dir.join("codex-code-mode-host");
        std::os::unix::fs::symlink(real.join("codex-code-mode-host"), &host).unwrap();
        let real = std::fs::canonicalize(&real).unwrap();

        let paths = binaries(&bin);
        for want in [
            &bin,
            &host,
            &real.join("codex"),
            &real.join("codex-code-mode-host"),
        ] {
            assert!(paths.contains(want), "{want:?} missing from {paths:?}");
        }

        std::fs::remove_file(&host).unwrap();
        assert_eq!(binaries(&bin), vec![real.join("codex"), bin]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mounts_codex_config_but_the_sessions_own_state() {
        let base = std::env::temp_dir().join(format!("mothership-ch-{}", std::process::id()));
        let home = base.join("codex");
        // The home's own sessions and a link where a session directory goes are both replaced.
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::create_dir_all(home.join("skills")).unwrap();
        std::os::unix::fs::symlink("/elsewhere", home.join("log")).unwrap();
        for file in [
            "auth.json",
            "config.toml",
            "models_cache.json",
            "state_5.sqlite",
            "state_5.sqlite-wal",
        ] {
            std::fs::write(home.join(file), "").unwrap();
        }
        std::os::unix::fs::symlink("/elsewhere/AGENTS.md", home.join("AGENTS.md")).unwrap();
        let codex = CodexHome {
            home: home.clone(),
            state: base.join("state/s1"),
        };
        let h = |rel: &str| home.join(rel).display().to_string();
        let mut want = vec!["--tmpfs".to_string(), home.display().to_string()];
        want.extend([
            "--symlink".into(),
            "/elsewhere/AGENTS.md".into(),
            h("AGENTS.md"),
        ]);
        for (kind, rel) in [
            ("--bind", "auth.json"),
            ("--ro-bind", "config.toml"),
            ("--ro-bind", "skills"),
        ] {
            want.extend([kind.into(), h(rel), h(rel)]);
        }
        for dir in HOME_DIRS {
            let own = codex.state.join(dir).display().to_string();
            want.extend(["--bind".into(), own, h(dir)]);
        }
        assert_eq!(home_mounts(&codex), want);
        assert!(codex.state.join("sessions").is_dir());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn writes_codex_state_but_not_its_config() {
        let writes = home_writes(Path::new("/h/.co dex"));
        assert!(writes.contains(&"(subpath \"/h/.co dex/sessions\")".to_string()));
        assert!(writes.contains(&"(literal \"/h/.co dex/auth.json\")".to_string()));
        assert!(writes.contains(
            &r#"(regex #"^/h/\.co\ dex/[^/]*\.sqlite(-shm|-wal|-journal)?$")"#.to_string()
        ));
        assert!(
            !writes
                .iter()
                .any(|w| w.contains("config") || w.contains("hooks") || w.contains("AGENTS"))
        );
    }
}
