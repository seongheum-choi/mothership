//! The issue's git worktree: cut on the first turn, remembered, and cut again when it is gone.

use super::Linear;
use crate::{app::App, repos::Repo, store::SessionRec, worktree};
use anyhow::Result;
use std::path::PathBuf;

impl Linear {
    /// The issue's worktree of a git repository and the branch it is on, created on the
    /// first turn and remembered after that.
    pub(super) async fn worktree(
        &self,
        app: &App,
        key: &str,
        rec: &SessionRec,
        repo: &Repo,
    ) -> Result<(PathBuf, String)> {
        if let (Some(w), Some(b)) = (&rec.workspace, &rec.branch)
            && w.exists()
        {
            return Ok((w.clone(), b.clone()));
        }
        let branch = match recorded_branch(rec) {
            Some(b) => b.to_string(),
            None => match self.branch_name(app, &rec.issue_id).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("[{key}] branchName lookup failed ({e:#}), using identifier");
                    rec.identifier.to_lowercase()
                }
            },
        };
        let dir = app.cfg.worktrees_dir.join(&rec.identifier);
        let workspace = worktree::ensure(&repo.path, &dir, &branch, &repo.base_branch).await?;
        let branch = worktree::current_branch(&workspace).await.unwrap_or(branch);
        app.store.update(|s| {
            if let Some(r) = s.sessions.get_mut(key) {
                r.workspace = Some(workspace.clone());
                r.branch = Some(branch.clone());
            }
        });
        Ok((workspace, branch))
    }
}

/// The branch the session's worktree was first cut on, to cut it again on: closing the issue or
/// deleting the directory removes a worktree, never its branch. A branch the agent moved to
/// later, such as a stacked one, is not recorded, since cutting that one again would start the
/// session from the wrong base.
fn recorded_branch(rec: &SessionRec) -> Option<&str> {
    rec.branch.as_deref().filter(|b| !b.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lost_worktree_is_cut_again_on_its_branch() {
        let mut rec = SessionRec::default();
        assert_eq!(recorded_branch(&rec), None, "first turn: ask Linear");
        rec.branch = Some(String::new());
        assert_eq!(recorded_branch(&rec), None);
        rec.branch = Some("nate/sh-7".into());
        rec.workspace = Some(PathBuf::from("/gone/SH-7"));
        assert_eq!(recorded_branch(&rec), Some("nate/sh-7"));
    }
}
