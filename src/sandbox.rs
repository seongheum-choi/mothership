//! Keeps agents out of the rest of the home directory (SSH keys, cloud credentials, notes).

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Claude Code settings that deny reads of every home entry not on the way to `allowed`.
///
/// For each directory from `home` down to an allowed path, siblings that lead nowhere
/// allowed get a `Read` deny rule; an allowed path's own subtree stays open. Claude Code
/// applies these rules under `bypassPermissions` too, and checks path arguments of
/// reading shell commands (`cat`, `cp`, ...) against them.
pub fn settings(home: &Path, allowed: &[PathBuf], extra_deny: &[String]) -> Value {
    let mut deny = home_deny_rules(home, allowed);
    deny.extend_from_slice(extra_deny);
    json!({ "permissions": { "deny": deny } })
}

fn home_deny_rules(home: &Path, allowed: &[PathBuf]) -> Vec<String> {
    let targets: Vec<Vec<String>> = allowed
        .iter()
        .filter_map(|p| p.strip_prefix(home).ok())
        .map(|rel| {
            rel.iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .filter(|segments| !segments.is_empty())
        .collect();
    // With nothing allowed inside home, every entry of home is denied.
    let mut deny = Vec::new();
    walk(home, &targets, &mut deny);
    deny
}

fn walk(dir: &Path, targets: &[Vec<String>], deny: &mut Vec<String>) {
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
            let rule = if path.is_dir() {
                format!("Read(/{}/**)", path.display())
            } else {
                format!("Read(/{})", path.display())
            };
            deny.push(rule);
        } else if !below.iter().any(Vec::is_empty) {
            walk(&path, &below, deny);
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

        let rules = home_deny_rules(&home, &[home.join("work/repo")]);
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
            home_deny_rules(&home, &[PathBuf::from("/elsewhere")]),
            vec![
                format!("Read(/{h}/.netrc)"),
                format!("Read(/{h}/.ssh/**)"),
                format!("Read(/{h}/notes/**)"),
                format!("Read(/{h}/work/**)"),
            ]
        );
        std::fs::remove_dir_all(&home).unwrap();
    }
}
