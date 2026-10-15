//! A Codex run: `codex exec` with Codex's own sandbox off, since the profile is the sandbox.

use super::{Host, request, seatbelt};
use request::CodexRun;
use serde_json::Value;
use std::path::Path;

/// What a run may write in Codex's home: its sessions, logs, caches, state databases and a
/// refreshed login. Not its config, hooks, rules, skills, memories or binaries, which later
/// runs, the Codex runner and the user's own Codex would pick up.
pub fn home_writes(home: &Path) -> Vec<String> {
    let mut writes: Vec<String> = [
        "sessions",
        "thread-writer-locks",
        "archived_sessions",
        "log",
        "cache",
        "shell_snapshots",
        ".tmp",
        "tmp",
    ]
    .iter()
    .flat_map(|dir| seatbelt::subpath(&home.join(dir)))
    .collect();
    writes.extend(
        [
            "auth.json",
            "models_cache.json",
            "history.jsonl",
            "installation_id",
            ".codex-global-state.json",
            ".codex-global-state.json.bak",
            ".sqlite-maintenance.lock",
        ]
        .iter()
        .flat_map(|file| seatbelt::literal(&home.join(file))),
    );
    writes.push(seatbelt::regex(&format!(
        "^{}/[^/]*\\.sqlite(-shm|-wal|-journal)?$",
        seatbelt::escape(&home.to_string_lossy())
    )));
    writes
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
        let profile = self.profile(&self.codex_settings, dir, home_writes(&self.codex_home));
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
