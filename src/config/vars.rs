//! Raw setting values: the environment over `<home>/.env`, and the list formats they use.

use anyhow::{Result, anyhow, bail};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

pub(super) struct Vars {
    file: HashMap<String, String>,
    file_path: PathBuf,
}

impl Vars {
    pub(super) fn load(home: &Path) -> Self {
        let file_path = home.join(".env");
        let file = std::fs::read_to_string(&file_path)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
            .collect();
        Self { file, file_path }
    }

    /// The entries of `<home>/.env` as written, without the environment.
    pub(super) fn file(&self) -> &HashMap<String, String> {
        &self.file
    }

    pub(super) fn get(&self, key: &str) -> Option<String> {
        std::env::var(key)
            .ok()
            .or_else(|| self.file.get(key).cloned())
            .filter(|v| !v.is_empty())
    }

    pub(super) fn require(&self, key: &str) -> Result<String> {
        self.get(key).ok_or_else(|| {
            anyhow!(
                "missing required setting {key} (environment or {})",
                self.file_path.display()
            )
        })
    }

    /// Pair each key with its value, warning about any key that has none so a missing secret
    /// is visible at startup rather than as a silent agent failure.
    pub(super) fn resolve_keys(&self, keys: &[String]) -> Vec<(String, String)> {
        keys.iter()
            .filter_map(|key| {
                if let Some(value) = self.get(key) {
                    return Some((key.clone(), value));
                }
                tracing::warn!(
                    "AGENT_ENV lists {key}, but it has no value in the environment or {}",
                    self.file_path.display()
                );
                None
            })
            .collect()
    }
}

/// Split a comma-separated list (`AGENT_ENV`, `GITHUB_TRUSTED_LOGINS`): trimmed, empties
/// dropped, first occurrence of each kept.
pub(super) fn parse_list(list: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for key in list.split(',').map(str::trim).filter(|k| !k.is_empty()) {
        if !keys.iter().any(|seen| seen == key) {
            keys.push(key.to_string());
        }
    }
    keys
}

/// A comma-separated path list (`SANDBOX_READ`, `SANDBOX_WRITE`) with `~` standing for the
/// user's home, since the deny rules need absolute paths. The OS sandbox judges a symlink by
/// its target, so a symlink's resolved target is listed after it.
pub(super) fn parse_paths(key: &str, list: Option<&str>, user_home: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for p in parse_list(list.unwrap_or_default()) {
        let path = match p.strip_prefix('~') {
            Some("") => user_home.to_path_buf(),
            Some(rest) if rest.starts_with('/') => user_home.join(&rest[1..]),
            _ => PathBuf::from(&p),
        };
        if path == user_home {
            // The deny rules are per entry of home, so home itself cannot be an exception;
            // listing it would silently deny everything instead.
            bail!("{key} cannot list the home directory itself; name the paths inside it");
        }
        let target = std::fs::canonicalize(&path).ok().filter(|t| *t != path);
        paths.push(path);
        paths.extend(target);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::{parse_list, parse_paths};
    use std::path::{Path, PathBuf};

    #[test]
    fn parses_trimmed_nonempty_keys() {
        assert_eq!(
            parse_list("CLAUDE_CODE_OAUTH_TOKEN, CLAUDE_CODE_EFFORT_LEVEL"),
            ["CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_EFFORT_LEVEL"]
        );
    }

    #[test]
    fn drops_empty_entries() {
        assert_eq!(parse_list(" , A ,, B , "), ["A", "B"]);
    }

    #[test]
    fn deduplicates_keeping_first() {
        assert_eq!(parse_list("A,B,A"), ["A", "B"]);
    }

    #[test]
    fn empty_list_yields_no_keys() {
        let none: [String; 0] = [];
        assert_eq!(parse_list(""), none);
        assert_eq!(parse_list("   "), none);
    }

    #[test]
    fn parse_paths_expands_the_user_home() {
        let home = Path::new("/nonexistent/me");
        assert_eq!(
            parse_paths("K", Some("~/.cargo, /nonexistent/tools,~other/x"), home).unwrap(),
            [
                PathBuf::from("/nonexistent/me/.cargo"),
                PathBuf::from("/nonexistent/tools"),
                PathBuf::from("~other/x"),
            ]
        );
        assert_eq!(parse_paths("K", None, home).unwrap(), [] as [PathBuf; 0]);
    }

    #[test]
    fn parse_paths_refuses_the_home_directory_itself() {
        let home = Path::new("/nonexistent/me");
        let err = parse_paths("SANDBOX_READ", Some("~/.cargo,~"), home).unwrap_err();
        assert!(err.to_string().starts_with("SANDBOX_READ"), "{err}");
        assert!(parse_paths("K", Some("/nonexistent/me"), home).is_err());
    }

    #[test]
    fn parse_paths_adds_symlink_targets() {
        let home = std::env::temp_dir().join(format!("mothership-paths-{}", std::process::id()));
        std::fs::create_dir_all(home.join("settings/git")).unwrap();
        std::fs::write(home.join("settings/git/.gitconfig"), "").unwrap();
        std::os::unix::fs::symlink(
            home.join("settings/git/.gitconfig"),
            home.join(".gitconfig"),
        )
        .unwrap();
        // The temporary directory may itself sit behind a symlink (`/var` on macOS).
        let target = std::fs::canonicalize(home.join("settings/git/.gitconfig")).unwrap();
        let settings = std::fs::canonicalize(home.join("settings")).unwrap();

        let paths = parse_paths("K", Some("~/.gitconfig,~/settings"), &home).unwrap();
        let mut expected = vec![home.join(".gitconfig"), target, home.join("settings")];
        if settings != home.join("settings") {
            expected.push(settings);
        }
        assert_eq!(paths, expected);
        std::fs::remove_dir_all(&home).unwrap();
    }
}
