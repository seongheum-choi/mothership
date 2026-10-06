//! Where finished work goes for review. Today the agent drives the backend's own CLI from
//! its instructions; a backend that sends review feedback back (Gerrit stream-events,
//! GitHub review webhooks) adds its routes here.

use anyhow::bail;
use std::str::FromStr;

#[derive(Clone, Copy)]
pub enum ReviewBackend {
    GitHub,
}

impl ReviewBackend {
    /// Appended to issue sessions' system prompt unless `APPEND_SYSTEM_PROMPT_FILE` replaces it.
    pub fn instructions(self) -> &'static str {
        match self {
            Self::GitHub => {
                "When the change is done: commit, push the branch, open a pull request with \
                 `gh pr create`, and put the GitHub pull request URL in your final reply."
            }
        }
    }
}

impl FromStr for ReviewBackend {
    type Err = anyhow::Error;

    fn from_str(name: &str) -> anyhow::Result<Self> {
        match name.to_ascii_lowercase().as_str() {
            "github" => Ok(Self::GitHub),
            other => bail!("unknown REVIEW_BACKEND {other:?} (supported: github)"),
        }
    }
}
