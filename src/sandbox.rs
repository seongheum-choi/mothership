//! Keeps agents out of the rest of the home directory (SSH keys, cloud credentials, notes) and
//! from editing what later sessions run with in `MOTHERSHIP_HOME`.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Home paths every agent's commands need besides its own workspace: toolchains, package
/// caches, git config.
#[derive(Clone)]
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
    let git_dir = worktree_git_dir(workspace);
    let clone_git = clone_git(git_dir.as_deref());
    let writable: Vec<PathBuf> = std::iter::once(workspace.to_path_buf())
        .chain(clone_git.clone())
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
    if let (Some(clone_git), Some(git_dir)) = (&clone_git, &git_dir) {
        deny.extend(git_dir_rules(clone_git, git_dir));
    }
    deny.extend_from_slice(extra_deny);
    json!({
        "permissions": { "deny": deny },
        "sandbox": {
            "enabled": true,
            "allowUnsandboxedCommands": false,
            "failIfUnavailable": true,
            "filesystem": { "allowWrite": paths.write },
            // Local servers are how agents run and probe what they build; no outside port is involved.
            // trustd verifies TLS for Go binaries built without cgo, such as the `gh stack`
            // extension; without it every HTTPS call fails with `x509: OSStatus -26276`.
            "network": {
                "allowLocalBinding": true,
                "allowMachLookup": ["com.apple.trustd.agent"],
            },
            "credentials": {
                "envVars": [{ "name": "CLAUDE_CODE_OAUTH_TOKEN", "mode": "deny" }],
            },
        },
    })
}

/// The clone's `.git` of a linked worktree whose git directory is `git_dir`
/// (`<clone>/.git/worktrees/<name>`).
fn clone_git(git_dir: Option<&Path>) -> Option<PathBuf> {
    Some(git_dir?.parent()?.parent()?.to_path_buf())
}

/// The clone's `.git` that a commit from `workspace` writes, when it is a linked worktree.
pub fn workspace_clone_git(workspace: &Path) -> Option<PathBuf> {
    clone_git(worktree_git_dir(workspace).as_deref())
}

/// The git directory of `workspace` when it is a linked worktree: its `.git` is a file naming
/// `<clone>/.git/worktrees/<name>`.
fn worktree_git_dir(workspace: &Path) -> Option<PathBuf> {
    let gitfile = std::fs::read_to_string(workspace.join(".git")).ok()?;
    Some(PathBuf::from(gitfile.strip_prefix("gitdir:")?.trim()))
}

/// A commit from the worktree writes the clone's `.git` (objects, refs, the worktree's own
/// index under `worktrees/`), and the sandbox folds an `Edit` deny into Bash's writes, so the
/// clone's `.git` is left editable but for what would change later sessions: its config, its
/// hooks and the other worktrees' metadata.
fn git_dir_rules(clone_git: &Path, git_dir: &Path) -> Vec<String> {
    let mut rules = vec![
        format!("{EDIT}(/{}/config)", clone_git.display()),
        format!("{EDIT}(/{}/hooks/**)", clone_git.display()),
    ];
    let worktrees = clone_git.join("worktrees");
    rules.extend(deny_rules(&worktrees, &[git_dir.to_path_buf()], EDIT));
    rules
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

/// Empty `.env` files in the directories under `state` that hold workspaces (`dirs`), created
/// when missing and returned for `SANDBOX_READ`. dotenv loaders such as diesel's take the first
/// `.env` above the working directory; under `MOTHERSHIP_HOME` that is mothership's own, which
/// agents may not read, and the loader fails instead of moving on. A readable empty file stops
/// the search first. One that is already there is left as it is.
pub fn dotenv_stops(state: &Path, dirs: &[PathBuf]) -> Vec<PathBuf> {
    dirs.iter()
        .filter(|dir| dir.starts_with(state) && dir.as_path() != state)
        .filter_map(|dir| {
            let stop = dir.join(".env");
            let made = std::fs::create_dir_all(dir).and_then(|()| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&stop)
            });
            match made {
                Ok(_) => Some(stop),
                Err(err) => {
                    tracing::warn!("cannot create {}: {err}", stop.display());
                    None
                }
            }
        })
        .collect()
}

/// Limits Bash's network to the hosts in `<state>/allowed-domains`, one per line with `#`
/// comments, when that file exists; read per turn, so adding a host needs no restart. Without
/// the file, `bypassPermissions` lets Bash reach any host through the sandbox's proxy. An
/// unreadable file allows no host rather than every one.
pub fn restrict_network(settings: &mut Value, state: &Path) {
    let file = state.join("allowed-domains");
    let domains = match std::fs::read_to_string(&file) {
        Ok(text) => parse_domains(&text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            tracing::warn!("cannot read {}, allowing no host: {err}", file.display());
            Vec::new()
        }
    };
    let network = &mut settings["sandbox"]["network"];
    network["strictAllowlist"] = json!(true);
    network["allowedDomains"] = json!(domains);
}

fn parse_domains(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|domain| !domain.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_allowed_domains_without_comments() {
        let text = "# Anthropic\napi.anthropic.com\n\n*.npmjs.org  # npm\n  nodejs.org\n";
        assert_eq!(
            parse_domains(text),
            ["api.anthropic.com", "*.npmjs.org", "nodejs.org"]
        );
    }

    #[test]
    fn allowed_domains_file_turns_on_the_strict_allowlist() {
        let state = std::env::temp_dir().join(format!("mothership-net-{}", std::process::id()));
        std::fs::create_dir_all(&state).unwrap();
        let mut open = json!({ "sandbox": { "network": { "allowLocalBinding": true } } });
        restrict_network(&mut open, &state);
        assert!(open["sandbox"]["network"].get("strictAllowlist").is_none());

        std::fs::write(state.join("allowed-domains"), "github.com\n").unwrap();
        let mut strict = open.clone();
        restrict_network(&mut strict, &state);
        let network = &strict["sandbox"]["network"];
        assert_eq!(network["strictAllowlist"], true);
        assert_eq!(network["allowedDomains"], json!(["github.com"]));
        assert_eq!(network["allowLocalBinding"], true);
        std::fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn stops_dotenv_searches_below_the_state_dir() {
        let home = std::env::temp_dir().join(format!("mothership-dotenv-{}", std::process::id()));
        let state = home.join(".mothership");
        std::fs::create_dir_all(state.join("repos")).unwrap();
        std::fs::write(state.join("repos/.env"), "KEPT=1\n").unwrap();
        let outside = home.join("elsewhere");

        let stops = dotenv_stops(
            &state,
            &[
                state.join("worktrees"),
                state.join("repos"),
                outside.clone(),
                state.clone(),
            ],
        );
        assert_eq!(
            stops,
            vec![state.join("worktrees/.env"), state.join("repos/.env")]
        );
        assert_eq!(std::fs::read_to_string(&stops[0]).unwrap(), "");
        assert_eq!(std::fs::read_to_string(&stops[1]).unwrap(), "KEPT=1\n");
        assert!(!outside.exists());
        assert!(!state.join(".env").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

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

    #[test]
    fn leaves_the_clone_git_dir_editable_but_for_config_hooks_and_other_worktrees() {
        let home = std::env::temp_dir().join(format!("mothership-gitdir-{}", std::process::id()));
        let state = home.join(".mothership");
        let clone = state.join("repos/r");
        for dir in [
            "repos/r/src",
            "repos/r/.git/objects",
            "repos/r/.git/hooks",
            "repos/r/.git/worktrees/A",
            "repos/r/.git/worktrees/B",
            "worktrees/A",
            "worktrees/B",
        ] {
            std::fs::create_dir_all(state.join(dir)).unwrap();
        }
        std::fs::write(clone.join(".git/config"), "").unwrap();
        let git_dir = clone.join(".git/worktrees/A");
        let workspace = state.join("worktrees/A");
        std::fs::write(
            workspace.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        let paths = Paths {
            read: vec![],
            write: vec![],
            skip_deny: vec![],
        };

        let made = settings(
            &home,
            &state,
            &workspace,
            std::slice::from_ref(&clone),
            &[],
            &paths,
        );
        let deny: Vec<String> =
            serde_json::from_value(made["permissions"]["deny"].clone()).unwrap();
        let edits: Vec<String> = deny
            .into_iter()
            .filter(|r| r.starts_with("Edit("))
            .collect();
        let (s, c) = (state.display(), clone.display());
        assert_eq!(
            edits,
            vec![
                format!("Edit(/{s}/repos/r/src/**)"),
                format!("Edit(/{s}/worktrees/B/**)"),
                format!("Edit(/{c}/.git/config)"),
                format!("Edit(/{c}/.git/hooks/**)"),
                format!("Edit(/{c}/.git/worktrees/B/**)"),
            ]
        );
        // A workspace that is not a linked worktree (Zulip's) gets no git rule.
        let plain = settings(&home, &state, &state.join("worktrees/B"), &[], &[], &paths);
        let deny = plain["permissions"]["deny"].to_string();
        assert!(!deny.contains(".git"), "{deny}");
        std::fs::remove_dir_all(&home).unwrap();
    }
}
