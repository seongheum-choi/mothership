//! Where finished work goes for review. The agent drives the backend's own CLI from its
//! instructions. Review feedback comes back through `github` or `gerrit`, which find the session
//! it continues and hand it to [`intake`].

pub mod intake;
pub mod text;

use crate::repos::Repo;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    str::FromStr,
};

/// `REVIEW_BACKEND`, or a `repos.json` entry's `review_backend`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewBackend {
    GitHub,
    Gerrit,
}

impl ReviewBackend {
    /// Appended to issue sessions' system prompt unless `APPEND_SYSTEM_PROMPT_FILE` replaces it.
    pub fn instructions(self) -> &'static str {
        match self {
            Self::GitHub => {
                "When the change is done: commit, push the branch, open a pull request with \
                 `gh pr create`, and put the GitHub pull request URL in your final reply."
            }
            // The topic routes review feedback back to the issue's newest session.
            Self::Gerrit => {
                "When the change is done: commit it with the `Change-Id` trailer the \
                 repository's commit-msg hook adds, push it for review with `git push origin \
                 HEAD:refs/for/<base>%topic=<issue key>`, where `<base>` is the branch this \
                 worktree was cut from and `<issue key>` is the issue's key exactly as above, \
                 and put the Gerrit change URL in your final reply. To change it after review, \
                 amend the commit, keeping its Change-Id, and push it the same way: that \
                 uploads a new patch set. To point at a line on a change, explaining it or \
                 asking about it, pipe the text into `gerrit-reply <change> <patch-set> --new \
                 <path> <line>`, which leaves it as an unresolved comment."
            }
        }
    }
}

impl FromStr for ReviewBackend {
    type Err = anyhow::Error;

    fn from_str(name: &str) -> anyhow::Result<Self> {
        match name.to_ascii_lowercase().as_str() {
            "github" => Ok(Self::GitHub),
            "gerrit" => Ok(Self::Gerrit),
            other => bail!("unknown REVIEW_BACKEND {other:?} (supported: github, gerrit)"),
        }
    }
}

/// Writes the reply wrapper `script` to `<home>/<name>/<name>` and returns its directory.
pub fn install_reply(home: &Path, name: &str, script: &str) -> Result<PathBuf> {
    // A directory of its own: it goes first on agents' PATH, so nothing else may live there.
    let bin_dir = home.join(name);
    std::fs::create_dir_all(&bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;
    let script_path = bin_dir.join(name);
    std::fs::write(&script_path, script)
        .with_context(|| format!("writing {}", script_path.display()))?;
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("making {} executable", script_path.display()))?;
    Ok(bin_dir)
}

/// Warns about the git repositories that review in Gerrit while `GERRIT_URL` is unset: their
/// agents get no Gerrit credentials, and no review of theirs is heard.
pub fn warn_without_gerrit(repos: &[Repo], instance: ReviewBackend, gerrit_url: bool) {
    let names: Vec<&str> = repos
        .iter()
        .filter(|r| r.git && r.review_backend(instance) == ReviewBackend::Gerrit)
        .map(|r| r.name.as_str())
        .collect();
    if !gerrit_url && !names.is_empty() {
        tracing::warn!(
            "settings: {} review in Gerrit but GERRIT_URL is unset: agents get no Gerrit \
             credentials and Gerrit reviews are not heard",
            names.join(", ")
        );
    }
}
