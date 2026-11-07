//! Where finished work goes for review. The agent drives the backend's own CLI from its
//! instructions. Review feedback comes back through `github` or `gerrit`.

use anyhow::bail;
use std::str::FromStr;

#[derive(Clone, Copy)]
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
                 uploads a new patch set."
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
