//! bubblewrap (`bwrap`) arguments carrying the same file rules as a session's Claude Code
//! settings, for what mothership runs outside Claude Code's sandbox on Linux: the Seatbelt
//! profile's counterpart (`seatbelt.rs`), made of mounts instead of rules.

use super::seatbelt::{self, PROTECTED_DIRS, PROTECTED_FILES};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// How many levels under a directory a run may write [`guards`] looks for protected names and
/// `.git`s.
const DEPTH: usize = 3;

/// What a run may do at a path and everything below it, until a deeper mount says otherwise.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Access {
    Hidden,
    ReadOnly,
    Writable,
}

/// The arguments before the program for `settings` (see [`crate::sandbox::settings`]), with
/// [`seatbelt::profile`]'s rules:
/// - the filesystem is read-only, and what its `Read(…)` deny rules name is hidden: a directory
///   behind an empty read-only one, a file behind `/dev/null`;
/// - `workspace` (dropped when the bare `Edit` rule makes the session read-only), its
///   `allowWrite` paths and a fresh `/dev` are writable;
/// - its `Edit(…)` deny rules, and the names Claude Code's sandbox never lets Bash write
///   ([`PROTECTED_FILES`], [`PROTECTED_DIRS`]), are read-only again inside those;
/// - `own` (the run's own directory) is readable and writable whatever the rules say;
/// - every directory it may write in, and every `.git` in them, is a mount point, which
///   cannot be removed, renamed or replaced;
/// - in a PID namespace of its own, signals reach only processes in the same sandbox, and
///   nothing it started outlives it;
/// - `codex`, Codex's home for a Codex run, is laid out by [`super::codex::home_mounts`].
///
/// Mounts follow links, so each path counts as resolved, and a rule naming nothing that
/// exists has nothing to mount. The network stays open, as under Claude Code's sandbox with
/// `bypassPermissions`.
// shortcut: rules with globs are skipped, and protected names and `.git`s are found only
// `DEPTH` levels deep and only when they exist, so a run may create a missing `.envrc`; expand
// globs and watch for new names (fanotify) once a mode or a repository needs them.
pub fn args(
    settings: &Value,
    workspace: &[PathBuf],
    own: &Path,
    codex: Option<&super::CodexHome>,
) -> Vec<String> {
    let deny = seatbelt::denied(settings);
    let paths = |tool| -> Vec<PathBuf> {
        deny.iter()
            .filter_map(|rule| target(rule, tool))
            .filter_map(resolve)
            .collect()
    };
    let roots: Vec<PathBuf> = seatbelt::writable_roots(settings, workspace)
        .iter()
        .filter_map(resolve)
        .collect();
    let own = resolve(own).unwrap_or_else(|| own.to_path_buf());
    let mut fixed = paths("Edit");
    let mut pinned: BTreeSet<PathBuf> = roots.iter().cloned().collect();
    pinned.insert(own.clone());
    for root in &roots {
        guards(root, DEPTH, &mut fixed, &mut pinned);
    }
    let rules = Rules {
        hidden: paths("Read"),
        writable: roots,
        fixed,
        own,
    };

    let mut args: Vec<String> = [
        "--die-with-parent",
        "--new-session",
        "--unshare-pid",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
    ]
    .map(String::from)
    .to_vec();
    // Parents sort before what is inside them, so a deeper mount lands on top.
    let edges: BTreeSet<&PathBuf> = (rules.hidden.iter())
        .chain(&rules.writable)
        .chain(&rules.fixed)
        .chain(&pinned)
        .collect();
    let mut emptied = Vec::new();
    for path in edges {
        let access = rules.access(path);
        let above = path.parent().map_or(Access::ReadOnly, |p| rules.access(p));
        if access != above || pinned.contains(path) {
            mount(&mut args, path, access, &mut emptied);
        }
    }
    args.extend(codex.map(super::codex::home_mounts).unwrap_or_default());
    // Last, once the mounts inside them have their mount points.
    for dir in emptied {
        args.extend(["--remount-ro".to_string(), dir]);
    }
    args.push("--".into());
    args
}

/// The path rules of one session, resolved.
struct Rules {
    hidden: Vec<PathBuf>,
    writable: Vec<PathBuf>,
    /// The `Edit` deny rules and protected names: read-only even where `writable` says otherwise.
    fixed: Vec<PathBuf>,
    own: PathBuf,
}

impl Rules {
    /// The same precedence as the Seatbelt profile's order of rules.
    fn access(&self, path: &Path) -> Access {
        let under = |paths: &[PathBuf]| paths.iter().any(|p| path.starts_with(p));
        if path.starts_with(&self.own) {
            Access::Writable
        } else if under(&self.hidden) {
            Access::Hidden
        } else if under(&self.writable) && !under(&self.fixed) {
            Access::Writable
        } else {
            Access::ReadOnly
        }
    }
}

fn mount(args: &mut Vec<String>, path: &Path, access: Access, emptied: &mut Vec<String>) {
    let p = path.to_string_lossy().into_owned();
    let (kind, source) = match access {
        Access::Writable => ("--bind", p.clone()),
        Access::ReadOnly => ("--ro-bind", p.clone()),
        Access::Hidden if path.is_dir() => {
            args.extend(["--tmpfs".into(), p.clone()]);
            emptied.push(p);
            return;
        }
        Access::Hidden => ("--ro-bind", "/dev/null".into()),
    };
    args.extend([kind.into(), source, p]);
}

/// The path a `<tool>(//abs/path)` or `<tool>(//abs/dir/**)` rule confines; none for a glob
/// or a rule without an absolute path.
fn target(rule: &str, tool: &str) -> Option<PathBuf> {
    let path = seatbelt::rule_path(rule, tool)?;
    let path = path.strip_suffix("/**").unwrap_or(path);
    (!path.contains('*')).then(|| PathBuf::from(path))
}

fn resolve(path: impl AsRef<Path>) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// Under `dir`, `depth` levels deep and not following links: what is protected (into `fixed`,
/// not looked into further) and every `.git` (into `pinned`), so a `.git` cannot be moved
/// aside and back with hooks of its own.
fn guards(dir: &Path, depth: usize, fixed: &mut Vec<PathBuf>, pinned: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if protected(&path) {
            fixed.push(path);
            continue;
        }
        if entry.file_name() == ".git" {
            pinned.insert(path.clone());
        }
        if kind.is_dir() && depth > 1 {
            guards(&path, depth - 1, fixed, pinned);
        }
    }
}

fn protected(path: &Path) -> bool {
    let path = path.to_string_lossy();
    (PROTECTED_FILES.iter())
        .chain(&PROTECTED_DIRS)
        .any(|name| path.ends_with(&format!("/{name}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree(name: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("mothership-bw-{name}-{}", std::process::id()));
        for dir in [
            "ws/sub/.git/hooks",
            "ws/locked",
            "ws/.claude",
            "secret/own",
            "sess",
        ] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        for file in ["ws/.git", "ws/.envrc", "ws/a.rs", "netrc"] {
            std::fs::write(base.join(file), "").unwrap();
        }
        std::fs::canonicalize(&base).unwrap()
    }

    fn settings(d: &Path, extra: &[&str]) -> Value {
        let mut deny = vec![
            format!("Read(/{}/secret/**)", d.display()),
            format!("Read(/{}/netrc)", d.display()),
            format!("Read(/{}/missing/**)", d.display()),
            format!("Edit(/{}/ws/locked/**)", d.display()),
            "Read(//**/.env)".into(),
            "Bash(git push:*)".into(),
        ];
        deny.extend(extra.iter().map(ToString::to_string));
        json!({
            "permissions": {"deny": deny},
            "sandbox": {"filesystem": {"allowWrite": [d.join("sess")]}},
        })
    }

    #[test]
    fn mounts_the_sessions_rules() {
        let d = tree("rules");
        let made = args(
            &settings(&d, &[]),
            &[d.join("ws")],
            &d.join("secret/own"),
            None,
        );
        let p = |rel: &str| d.join(rel).display().to_string();
        let mut want: Vec<String> = [
            "--die-with-parent",
            "--new-session",
            "--unshare-pid",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
        ]
        .map(String::from)
        .to_vec();
        for (kind, rel) in [
            ("--ro-bind /dev/null", "netrc"),
            ("--tmpfs", "secret"),
            ("--bind =", "secret/own"),
            ("--bind =", "sess"),
            ("--bind =", "ws"),
            ("--ro-bind =", "ws/.claude"),
            ("--ro-bind =", "ws/.envrc"),
            ("--bind =", "ws/.git"),
            ("--ro-bind =", "ws/locked"),
            ("--bind =", "ws/sub/.git"),
            ("--ro-bind =", "ws/sub/.git/hooks"),
            ("--remount-ro", "secret"),
        ] {
            for word in kind.split(' ') {
                want.push(if word == "=" { p(rel) } else { word.into() });
            }
            want.push(p(rel));
        }
        want.push("--".into());
        assert_eq!(made, want);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_read_only_session_writes_only_its_own() {
        let d = tree("ro");
        let made = args(
            &settings(&d, &["Edit"]),
            &[d.join("ws")],
            &d.join("secret/own"),
            None,
        );
        let ws = d.join("ws").display().to_string();
        assert!(!made.contains(&ws), "{made:?}");
        let bound: Vec<&[String]> = made.windows(2).filter(|w| w[0] == "--bind").collect();
        assert_eq!(bound.len(), 2, "{made:?}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn reads_rule_targets() {
        assert_eq!(target("Read(//h/.ssh/**)", "Read"), Some("/h/.ssh".into()));
        assert_eq!(target("Edit(//h/x)", "Edit"), Some("/h/x".into()));
        assert_eq!(target("Edit(//h/x)", "Read"), None);
        assert_eq!(target("Read(//h/*.pem)", "Read"), None);
        assert_eq!(target("Read(src/**)", "Read"), None);
        assert!(protected(Path::new("/w/sub/.git/config")));
        assert!(protected(Path::new("/w/lefthook.yaml")));
        assert!(!protected(Path::new("/w/.idea")));
        assert!(!protected(Path::new("/w/x.envrc")));
    }
}
