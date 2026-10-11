//! Keeps agents out of the rest of the home directory (SSH keys, cloud credentials, notes) and
//! from editing what later sessions run with in `MOTHERSHIP_HOME`.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Home paths every agent's commands need besides its own workspace: toolchains, package
/// caches, git config.
pub struct Paths {
    /// `SANDBOX_READ`: left out of the deny rules, so files work as well as directories.
    pub read: Vec<PathBuf>,
    /// `SANDBOX_WRITE` and the progress directory: writable from Bash on top of the working
    /// directory, and readable like `read`.
    pub write: Vec<PathBuf>,
    /// `SANDBOX_SKIP_DENY`: entries of home that get no deny rule at all, so the Read tool
    /// can read them. On macOS, a deny rule on Desktop, Documents or Downloads makes Claude
    /// Code touch the folder, which raises a privacy dialog that blocks an unapproved
    /// background agent forever.
    pub skip_deny: Vec<PathBuf>,
}

/// Claude Code settings that deny reads of every home entry not on the way to `workspace`,
/// `readable`, `paths.read`, `paths.write` or `paths.skip_deny`, deny edits of everything in
/// `state` (`MOTHERSHIP_HOME`) but `workspace` and `paths.write`, and run Bash
/// in Claude Code's OS sandbox.
///
/// For each directory from `home` down to an allowed path, siblings that lead nowhere
/// allowed get a `Read` deny rule; an allowed path's own subtree stays open. The rules stop
/// the Read tool, and the sandbox (Seatbelt on macOS, bubblewrap on Linux) folds them into
/// its filesystem policy, so they bind every process Bash starts as well, not only the
/// reading commands Claude Code recognises. Writes from Bash are limited to the working
/// directory and `paths.write`. Without a sandbox the session fails instead of running
/// unconfined, and the OAuth token is kept out of Bash's environment.
///
/// The `Edit` rules, which Claude Code applies to every file-editing tool, keep an agent from
/// changing what later sessions run with: `.env`, `repos.json`, `modes/`, `plugins/` and other
/// sessions' worktrees. Bash's writes are already confined by the sandbox.
pub fn settings(
    home: &Path,
    state: &Path,
    workspace: &Path,
    readable: &[PathBuf],
    extra_deny: &[String],
    paths: &Paths,
) -> Value {
    let writable: Vec<PathBuf> = std::iter::once(workspace.to_path_buf())
        .chain(paths.write.iter().cloned())
        .collect();
    let allowed: Vec<PathBuf> = writable
        .iter()
        .chain(readable)
        .chain(&paths.read)
        .chain(&paths.skip_deny)
        .cloned()
        .collect();
    let mut deny = deny_rules(home, &allowed, READ);
    deny.extend(deny_rules(state, &writable, EDIT));
    deny.extend_from_slice(extra_deny);
    json!({
        "permissions": { "deny": deny },
        "sandbox": {
            "enabled": true,
            "allowUnsandboxedCommands": false,
            "failIfUnavailable": true,
            "filesystem": { "allowWrite": paths.write },
            // Local servers are how agents run and probe what they build; no outside port is involved.
            "network": { "allowLocalBinding": true },
            "credentials": {
                "envVars": [{ "name": "CLAUDE_CODE_OAUTH_TOKEN", "mode": "deny" }],
            },
        },
    })
}

const READ: &str = "Read";
/// Claude Code checks file edits against `Edit` rules only; a `Write` or `NotebookEdit` rule is
/// never consulted and warns at startup.
const EDIT: &str = "Edit";

/// `tool` rules for every entry under `root` that is not on the way to `allowed`.
fn deny_rules(root: &Path, allowed: &[PathBuf], tool: &str) -> Vec<String> {
    let targets: Vec<Vec<String>> = allowed
        .iter()
        .filter_map(|p| p.strip_prefix(root).ok())
        .map(|rel| {
            rel.iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .filter(|segments| !segments.is_empty())
        .collect();
    // With nothing allowed inside root, every entry of root is denied.
    let mut deny = Vec::new();
    walk(root, &targets, tool, &mut deny);
    deny
}

fn walk(dir: &Path, targets: &[Vec<String>], tool: &str, deny: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let below: Vec<Vec<String>> = targets
            .iter()
            .filter(|t| t[0] == name)
            .map(|t| t[1..].to_vec())
            .collect();
        if below.is_empty() {
            // Claude Code permission patterns take absolute paths with a leading `//`.
            let pattern = if path.is_dir() {
                format!("/{}/**", path.display())
            } else {
                format!("/{}", path.display())
            };
            deny.push(format!("{tool}({pattern})"));
        } else if !below.iter().any(Vec::is_empty) {
            walk(&path, &below, tool, deny);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_everything_off_the_allowed_paths() {
        let home = std::env::temp_dir().join(format!("mothership-sandbox-{}", std::process::id()));
        for dir in [".ssh", "work/repo/src", "work/other", "notes"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        std::fs::write(home.join(".netrc"), "").unwrap();

        let rules = deny_rules(&home, &[home.join("work/repo")], READ);
        let h = home.display();
        assert_eq!(
            rules,
            vec![
                format!("Read(/{h}/.netrc)"),
                format!("Read(/{h}/.ssh/**)"),
                format!("Read(/{h}/notes/**)"),
                format!("Read(/{h}/work/other/**)"),
            ]
        );
        assert_eq!(
            deny_rules(&home, &[PathBuf::from("/elsewhere")], READ),
            vec![
                format!("Read(/{h}/.netrc)"),
                format!("Read(/{h}/.ssh/**)"),
                format!("Read(/{h}/notes/**)"),
                format!("Read(/{h}/work/**)"),
            ]
        );
        assert_eq!(
            deny_rules(&home, &[home.join("work/repo"), home.join(".netrc")], READ),
            vec![
                format!("Read(/{h}/.ssh/**)"),
                format!("Read(/{h}/notes/**)"),
                format!("Read(/{h}/work/other/**)"),
            ]
        );
        assert_eq!(
            deny_rules(&home.join("work"), &[home.join("work/repo")], EDIT),
            vec![format!("Edit(/{h}/work/other/**)")]
        );
        std::fs::remove_dir_all(&home).unwrap();
    }
}
