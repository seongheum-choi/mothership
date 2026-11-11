//! Where finished work goes for review. The agent drives the backend's own CLI from its
//! instructions. Review feedback comes back through `github` or `gerrit`, which find the session
//! it continues and hand it to [`intake`].

pub mod intake;

use crate::repos::Repo;
use anyhow::bail;
use serde::Deserialize;
use std::str::FromStr;

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
