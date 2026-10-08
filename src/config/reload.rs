//! Settings that change without a restart: the repository list, re-read from `<home>/repos.json`
//! (or `REPO_PATH`/`BASE_BRANCH`) when that file or `<home>/.env` has a new modification time.
//! Any other `.env` key is read once at startup, so a change to it is only logged.

use super::vars::Vars;
use crate::repos::Repo;
use anyhow::Result;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::SystemTime,
};

/// The `.env` keys a reload applies; every other key needs a restart.
const HOT_KEYS: &[&str] = &["REPO_PATH", "BASE_BRANCH"];

/// Modification times of `repos.json` and `.env`; `None` for a missing file.
#[derive(Clone, Copy, Default, PartialEq)]
struct Stamps {
    repos: Option<SystemTime>,
    env: Option<SystemTime>,
}

impl Stamps {
    fn read(home: &Path) -> Self {
        let mtime = |name: &str| {
            std::fs::metadata(home.join(name))
                .and_then(|m| m.modified())
                .ok()
        };
        Self {
            repos: mtime("repos.json"),
            env: mtime(".env"),
        }
    }
}

/// What the last load saw: the files' times and the `.env` entries.
struct Seen {
    stamps: Stamps,
    env: HashMap<String, String>,
}

/// The repository list in effect, replaced whole so a turn keeps the one it started with.
pub struct Live {
    home: PathBuf,
    user_home: PathBuf,
    repos: RwLock<Arc<Vec<Repo>>>,
    seen: Mutex<Seen>,
}

impl Live {
    /// Loads the repository list at startup, where a broken one is an error.
    pub fn load(home: &Path, user_home: &Path) -> Result<Self> {
        let stamps = Stamps::read(home);
        let vars = Vars::load(home);
        let repos = load_repos(&vars, home, user_home)?;
        Ok(Self {
            home: home.to_path_buf(),
            user_home: user_home.to_path_buf(),
            repos: RwLock::new(Arc::new(repos)),
            seen: Mutex::new(Seen {
                stamps,
                env: vars.file().clone(),
            }),
        })
    }

    pub fn repos(&self) -> Arc<Vec<Repo>> {
        self.repos.read().expect("repository list poisoned").clone()
    }

    /// Reloads when `repos.json` or `.env` changed since the last look, and returns the new
    /// repository list if it differs. A list that fails to load is logged and the current one
    /// kept; the times are recorded either way, so a broken file is reported once.
    pub fn refresh(&self) -> Option<Arc<Vec<Repo>>> {
        let mut seen = self.seen.lock().expect("settings stamps poisoned");
        let stamps = Stamps::read(&self.home);
        if stamps == seen.stamps {
            return None;
        }
        seen.stamps = stamps;
        let vars = Vars::load(&self.home);
        let overridden = |key: &str| std::env::var_os(key).is_some();
        for key in changed_keys(&seen.env, vars.file(), overridden) {
            tracing::warn!(
                "settings: {key} changed in {}; restart mothership to apply it",
                self.home.join(".env").display()
            );
        }
        seen.env.clone_from(vars.file());
        let repos = match load_repos(&vars, &self.home, &self.user_home) {
            Ok(repos) => repos,
            Err(e) => {
                tracing::warn!("settings: {e:#}; keeping the current repositories");
                return None;
            }
        };
        let mut current = self.repos.write().expect("repository list poisoned");
        if **current == repos {
            return None;
        }
        *current = Arc::new(repos);
        tracing::info!(
            "settings: repositories reloaded: {}",
            current
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        Some(current.clone())
    }
}

/// From `<home>/repos.json`, or the one `REPO_PATH`/`BASE_BRANCH` describe. Never empty.
fn load_repos(vars: &Vars, home: &Path, user_home: &Path) -> Result<Vec<Repo>> {
    Ok(
        match crate::repos::load(&home.join("repos.json"), user_home)? {
            Some(repos) => repos,
            None => vec![Repo::single(
                PathBuf::from(vars.require("REPO_PATH")?),
                vars.get("BASE_BRANCH").unwrap_or_else(|| "main".into()),
            )],
        },
    )
}

/// Keys added, removed or changed between two readings of `.env`, sorted, leaving out the hot
/// ones and those the process environment sets, since the environment wins over the file.
fn changed_keys(
    old: &HashMap<String, String>,
    new: &HashMap<String, String>,
    overridden: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut keys: Vec<String> = old
        .keys()
        .chain(new.keys())
        .filter(|key| old.get(*key) != new.get(*key))
        .filter(|key| !HOT_KEYS.contains(&key.as_str()) && !overridden(key))
        .cloned()
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

#[cfg(test)]
mod tests {
    use super::changed_keys;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn changed_keys_lists_added_removed_and_changed_keys() {
        let old = env(&[("BIND", "a"), ("ZULIP_SITE", "z"), ("AGENT_NAME", "m")]);
        let new = env(&[
            ("BIND", "b"),
            ("AGENT_NAME", "m"),
            ("LINEAR_CLIENT_ID", "c"),
        ]);
        assert_eq!(
            changed_keys(&old, &new, |_| false),
            ["BIND", "LINEAR_CLIENT_ID", "ZULIP_SITE"]
        );
    }

    #[test]
    fn changed_keys_leaves_out_hot_and_overridden_keys() {
        let old = env(&[("REPO_PATH", "/a"), ("BIND", "a")]);
        let new = env(&[("REPO_PATH", "/b"), ("BASE_BRANCH", "dev"), ("BIND", "b")]);
        assert_eq!(changed_keys(&old, &new, |k| k == "BIND"), [] as [String; 0]);
        assert_eq!(changed_keys(&old, &old, |_| false), [] as [String; 0]);
    }
}
