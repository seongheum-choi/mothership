//! Where a session from before repository routing works: the repository its worktree was cut
//! from, or once that worktree is gone, the one that has its branch.

use super::{Linear, routing::Blocked};
use crate::{app::App, repos::Repo, sessions};
use std::{path::Path, sync::Arc};

impl Linear {
    /// Settles the repository of a session that recorded a worktree or a branch but no
    /// repository. `Ok(false)` when neither names one, so the issue routes it.
    pub(super) async fn choose_pre_routing(
        &self,
        app: &Arc<App>,
        sid: &str,
        repos: &[Repo],
        workspace: Option<&Path>,
        branch: Option<&str>,
    ) -> Result<bool, Blocked> {
        if let Some(workspace) = workspace.filter(|w| w.exists()) {
            let Some(repo) = sessions::workspace_repo(repos, workspace).await else {
                return Err(Blocked::Stuck(format!(
                    "This session's worktree {} belongs to no repository in repos.json. Add \
                     its repository and reply to continue.",
                    workspace.display()
                )));
            };
            self.settle(app, sid, repo, "the repository of this session's worktree")
                .await;
            return Ok(true);
        }
        // A gone worktree (removed by hand, or by closed-issue cleanup, which also clears the
        // record) says nothing about its repository, but its branch outlives it. Issue routing
        // could pick another clone and start a new branch there, away from the work.
        let Some(branch) = branch.filter(|b| !b.is_empty()) else {
            return Ok(false);
        };
        let Some(repo) = sessions::branch_repo(repos, branch).await else {
            return Ok(false);
        };
        let reason = format!("the repository with this session's branch `{branch}`");
        self.settle(app, sid, repo, &reason).await;
        Ok(true)
    }
}
