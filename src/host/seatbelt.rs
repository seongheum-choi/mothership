//! A macOS Seatbelt profile (for `sandbox-exec -p`) carrying the same file rules as a
//! session's Claude Code settings, for what mothership runs outside Claude Code's sandbox.

use serde_json::Value;
use std::{
    fmt::Write,
    path::{Path, PathBuf},
};

/// The profile for `settings` (see [`crate::sandbox::settings`]):
/// - its `Read(…)` deny rules deny reading those paths;
/// - writing is denied everywhere but `workspace` (dropped when the bare `Edit` rule makes
///   the session read-only), its `allowWrite` paths and the `always` filters;
/// - its `Edit(…)` deny rules deny writing again inside those (a clone's `config` and
///   `hooks`, other sessions' worktrees), and so do the names Claude Code's sandbox never lets
///   Bash write ([`PROTECTED`]), but for `.idea`;
/// - `own` (the run's own directory) is readable and writable whatever the rules say;
/// - no directory it may write in can itself be removed, renamed or replaced, nor any `.git`
///   moved away, since mothership reads and writes those paths again after the run, and a
///   `.git` moved aside and back would carry hooks past the `.git/hooks` rule;
/// - signals reach only processes in the same sandbox, as under Claude Code's.
///
/// Paths are matched as written and as resolved, since Seatbelt sees resolved paths. The
/// network and other services stay open, as under Claude Code's sandbox with
/// `bypassPermissions`.
pub fn profile(settings: &Value, workspace: &[PathBuf], always: &[String], own: &Path) -> String {
    let deny: Vec<&str> = settings["permissions"]["deny"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let read_only = deny.contains(&"Edit");
    let allow_write = settings["sandbox"]["filesystem"]["allowWrite"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(PathBuf::from);
    let roots: Vec<PathBuf> = (if read_only { &[][..] } else { workspace })
        .iter()
        .cloned()
        .chain(allow_write)
        .collect();
    let mut writable: Vec<String> = roots.iter().flat_map(|p| subpath(p)).collect();
    writable.extend(always.iter().cloned());
    let mut fixed: Vec<String> = roots.iter().flat_map(|p| literal(p)).collect();
    fixed.extend(literal(own));

    let mut sb = String::from("(version 1)\n(allow default)\n");
    sb.push_str("(deny signal)\n(allow signal (target same-sandbox))\n");
    let rules = |tool| {
        deny.iter()
            .flat_map(move |r| filters(r, tool))
            .collect::<Vec<_>>()
    };
    rule(&mut sb, "deny file-read*", &rules("Read"));
    sb.push_str("(deny file-write*)\n");
    rule(&mut sb, "allow file-write*", &writable);
    let mut protected = rules("Edit");
    protected.extend(PROTECTED.iter().map(|re| format!("(regex #\"{re}\")")));
    rule(&mut sb, "deny file-write*", &protected);
    rule(&mut sb, "allow file-read* file-write*", &subpath(own));
    rule(&mut sb, "deny file-write*", &fixed);
    sb.push_str("(deny file-write-unlink (regex #\"/\\.git$\"))\n");
    sb
}

/// What Claude Code 2.1's sandbox refuses Bash to write anywhere (its protected files, git
/// hooks and config, editor and agent config directories), as regexes. `.idea`, which it also
/// refuses, is left out: dependencies ship it, and refusing it is what fails `pnpm install`.
pub const PROTECTED: [&str; 3] = [
    "/\\.(gitconfig|gitmodules|bashrc|bash_profile|zshrc|zprofile|profile|zshenv|zlogin|zlogout|bash_login|bash_aliases|bash_logout|envrc|ripgreprc|mcp\\.json|claude\\.json|npmrc|yarnrc|yarnrc\\.yml|pnp\\.cjs|pnp\\.loader\\.mjs|pnpmfile\\.cjs|bunfig\\.toml|bazelrc|bazelversion|bazeliskrc|pre-commit-config\\.yaml|lefthook\\.yml|lefthook\\.yaml|devcontainer\\.json)$",
    "/(bunfig\\.toml|lefthook\\.ya?ml|gradle-wrapper\\.properties|maven-wrapper\\.properties|pyrightconfig\\.json)$",
    "/\\.(git/hooks(/|$)|git/config$|claude(/|$)|vscode(/|$)|devcontainer(/|$))",
];

/// One `(<action> <filter>…)` line; none when there are no filters, since an empty list
/// would apply to everything.
fn rule(sb: &mut String, action: &str, filters: &[String]) {
    if !filters.is_empty() {
        let _ = writeln!(sb, "({action} {})", filters.join(" "));
    }
}

/// The filters of a `<tool>(//abs/path)`, `<tool>(//abs/dir/**)` or `<tool>(//abs/**/glob)`
/// rule; other rules (`Bash(git push:*)`, project-relative patterns) have no path to confine.
fn filters(rule: &str, tool: &str) -> Vec<String> {
    let Some(path) = rule
        .strip_prefix(tool)
        .and_then(|r| r.strip_prefix("(/"))
        .and_then(|r| r.strip_suffix(')'))
        .filter(|p| p.starts_with('/'))
    else {
        return Vec::new();
    };
    match path.strip_suffix("/**") {
        Some(dir) if !dir.contains('*') => subpath(Path::new(dir)),
        _ if path.contains('*') => vec![regex(&glob(path))],
        _ => literal(Path::new(path)),
    }
}

/// `(subpath …)` for `path` and, when it differs, for what it resolves to.
pub fn subpath(path: &Path) -> Vec<String> {
    resolved(path)
        .iter()
        .map(|p| format!("(subpath {})", quote(&p.to_string_lossy())))
        .collect()
}

/// `(literal …)` for `path` and, when it differs, for what it resolves to.
pub fn literal(path: &Path) -> Vec<String> {
    resolved(path)
        .iter()
        .map(|p| format!("(literal {})", quote(&p.to_string_lossy())))
        .collect()
}

/// `(regex #"…")`: Seatbelt reads the pattern as written, so it is not quoted again; a `"`
/// in it is already escaped by [`glob`].
pub fn regex(pattern: &str) -> String {
    format!("(regex #\"{pattern}\")")
}

fn resolved(path: &Path) -> Vec<PathBuf> {
    let mut paths = vec![path.to_path_buf()];
    paths.extend(std::fs::canonicalize(path).ok().filter(|real| real != path));
    paths
}

/// A Claude Code path glob as an anchored regex: `**` crosses directories, `*` does not.
fn glob(pattern: &str) -> String {
    let mut re = String::from("^");
    let mut rest = pattern;
    while let Some(c) = rest.chars().next() {
        if let Some(after) = rest.strip_prefix("/**/") {
            re.push_str("/(.*/)?");
            rest = after;
        } else if rest == "/**" {
            re.push_str("(/.*)?");
            rest = "";
        } else if let Some(after) = rest.strip_prefix("**") {
            re.push_str(".*");
            rest = after;
        } else {
            if c == '*' {
                re.push_str("[^/]*");
            } else {
                re.push_str(&escape(&c.to_string()));
            }
            rest = &rest[c.len_utf8()..];
        }
    }
    re.push('$');
    re
}

/// `s` with every character a regex could read as syntax escaped.
/// A `"` cannot appear in a `#"…"` pattern even escaped, so it matches any character there:
/// a rule a little wider, never narrower.
pub fn escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => vec!['.'],
            c if c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-') => vec![c],
            c => vec!['\\', c],
        })
        .collect()
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(deny: &[&str]) -> Value {
        json!({
            "permissions": {"deny": deny},
            "sandbox": {"filesystem": {"allowWrite": ["/h/.cargo/registry", "/h/.m/progress/s1"]}},
        })
    }

    #[test]
    fn carries_the_sessions_rules_over() {
        let made = profile(
            &settings(&[
                "Read(//h/.ssh/**)",
                "Read(//h/.netrc)",
                "Edit(//h/.m/worktrees/B/**)",
                "Edit(//h/.m/repos/r/.git/config)",
                "Bash(git push:*)",
                "Read(src/**)",
            ]),
            &[PathBuf::from("/h/.m/worktrees/A")],
            &["(subpath \"/h/.codex/sessions\")".into()],
            Path::new("/h/.m/host-runs/r1"),
        );
        let protected: Vec<String> = PROTECTED
            .iter()
            .map(|re| format!("(regex #\"{re}\")"))
            .collect();
        assert_eq!(
            made,
            format!(
                "(version 1)\n(allow default)\n\
                 (deny signal)\n(allow signal (target same-sandbox))\n\
                 (deny file-read* (subpath \"/h/.ssh\") (literal \"/h/.netrc\"))\n\
                 (deny file-write*)\n\
                 (allow file-write* (subpath \"/h/.m/worktrees/A\") (subpath \"/h/.cargo/registry\") \
                 (subpath \"/h/.m/progress/s1\") (subpath \"/h/.codex/sessions\"))\n\
                 (deny file-write* (subpath \"/h/.m/worktrees/B\") (literal \"/h/.m/repos/r/.git/config\") {})\n\
                 (allow file-read* file-write* (subpath \"/h/.m/host-runs/r1\"))\n\
                 (deny file-write* (literal \"/h/.m/worktrees/A\") (literal \"/h/.cargo/registry\") \
                 (literal \"/h/.m/progress/s1\") (literal \"/h/.m/host-runs/r1\"))\n\
                 (deny file-write-unlink (regex #\"/\\.git$\"))\n",
                protected.join(" ")
            )
        );
    }

    #[test]
    fn a_read_only_session_writes_no_workspace() {
        let made = profile(
            &settings(&["Edit"]),
            &[PathBuf::from("/h/w")],
            &[],
            Path::new("/r"),
        );
        assert!(!made.contains("/h/w"), "{made}");
        assert!(
            made.contains("(allow file-write* (subpath \"/h/.cargo/registry\")"),
            "{made}"
        );
        assert!(!made.contains("deny file-read*"), "{made}");
    }

    #[test]
    fn quotes_globs_and_resolves_paths() {
        assert_eq!(
            filters(r#"Read(//h/a "b"\c/**)"#, "Read"),
            [r#"(subpath "/h/a \"b\"\\c")"#]
        );
        assert_eq!(filters("Read(/rel/**)", "Read"), Vec::<String>::new());
        assert_eq!(filters("Edit(//h/x)", "Read"), Vec::<String>::new());
        assert_eq!(
            filters("Read(//**/.env)", "Read"),
            [r#"(regex #"^/(.*/)?\.env$")"#]
        );
        assert_eq!(glob("/h/*.pem"), r"^/h/[^/]*\.pem$");
        assert_eq!(glob("/h/s/**/k*/**"), r"^/h/s/(.*/)?k[^/]*(/.*)?$");

        let dir = std::env::temp_dir().join(format!("mothership-sb-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let real = std::fs::canonicalize(dir.join("real")).unwrap();
        assert_eq!(
            subpath(&dir.join("link")),
            [
                format!("(subpath \"{}\")", dir.join("link").display()),
                format!("(subpath \"{}\")", real.display()),
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The profile as Seatbelt enforces it, on this Mac.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_enforces_the_profile() {
        let base = std::env::temp_dir().join(format!("mothership-sbx-{}", std::process::id()));
        for dir in ["ws/sub/.git/hooks", "own", "sess", "secret", "outside"] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        let d = std::fs::canonicalize(&base).unwrap();
        std::fs::write(d.join("secret/s"), "s").unwrap();
        let settings = json!({
            "permissions": {"deny": [format!("Read(/{}/secret/**)", d.display())]},
            "sandbox": {"filesystem": {"allowWrite": [d.join("sess")]}},
        });
        let made = profile(&settings, &[d.join("ws")], &[], &d.join("own"));
        let runs = |cmd: &str| {
            std::process::Command::new("/usr/bin/sandbox-exec")
                .args(["-p", &made, "/bin/sh", "-c", cmd])
                .current_dir(&d)
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        for allowed in [
            "echo > ws/a",
            "echo > own/x",
            "echo > sess/r.json",
            "echo > ws/sub/.git/x",
        ] {
            assert!(runs(allowed), "{allowed}");
        }
        for denied in [
            "cat secret/s",
            "echo > outside/x",
            "echo > ws/.envrc",
            "echo > ws/.ENVRC",
            "echo > ws/sub/.git/hooks/pre-commit",
            "mv ws/sub/.git ws/sub/.gitx",
            "rm -f sess/r.json && rmdir sess",
            "mv own own2",
            "mv ws ws2",
        ] {
            assert!(!runs(denied), "{denied}");
        }
        std::fs::remove_dir_all(&base).unwrap();
    }
}
