//! The session's git worktree: cut on the first turn, remembered, and cut again when it is gone.

use super::Linear;
use crate::{app::App, repos::Repo, store::SessionRec, worktree};
use anyhow::Result;
use std::path::{Path, PathBuf};

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
        let dir = issue_dir(
            &app.cfg.worktrees_dir,
            &dir_name(rec),
            &repo.path,
            &repo.name,
        )
        .await;
        let git_env = crate::gerrit::git_env(app.cfg.gerrit.as_ref());
        let workspace =
            worktree::ensure(&repo.path, &dir, &branch, &repo.base_branch, &git_env).await?;
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

/// The worktree's directory under `WORKTREES_DIR`: the issue's identifier, and for a session
/// opened for a pull request, its number after it, since the issue's own worktree is on
/// another branch.
fn dir_name(rec: &SessionRec) -> String {
    match &rec.pull_request {
        Some(pr) => format!("{}-pr{}", rec.identifier, pr.number),
        None => rec.identifier.clone(),
    }
}

/// `<root>/<name>`, unless a worktree of another repository holds it, as when two sessions of
/// one issue work in different repositories; then `<root>/<name>-<repository name>`. A worktree
/// of the same repository, from this clone or another clone of its `origin`, is reused, so an
/// instance with one repository keeps its paths.
async fn issue_dir(root: &Path, name: &str, clone: &Path, repo_name: &str) -> PathBuf {
    let dir = root.join(name);
    if worktree::of_another_repository(&dir, clone).await {
        return root.join(format!("{name}-{repo_name}"));
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// Two bare origins, `app` and `app-test`, with `app` cloned twice and `app-test` once.
    fn clones(tmp: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let _ = std::fs::remove_dir_all(tmp);
        std::fs::create_dir_all(tmp).expect("temp dir");
        // Git reports worktrees by their real path, `/private/tmp` for macOS's `/tmp`.
        let tmp = &tmp.canonicalize().expect("temp dir");
        for name in ["app", "app-test"] {
            git(tmp, &["init", "-q", "-b", "main", &format!("{name}-src")]);
            git(
                &tmp.join(format!("{name}-src")),
                &["commit", "-q", "--allow-empty", "-m", "first"],
            );
            git(
                tmp,
                &[
                    "clone",
                    "-q",
                    "--bare",
                    &format!("{name}-src"),
                    &format!("{name}.git"),
                ],
            );
        }
        git(tmp, &["clone", "-q", "app.git", "app"]);
        git(tmp, &["clone", "-q", "app.git", "app-2"]);
        git(tmp, &["clone", "-q", "app-test.git", "app-test"]);
        (tmp.join("app"), tmp.join("app-2"), tmp.join("app-test"))
    }

    async fn cut(root: &Path, clone: &Path, name: &str, branch: &str) -> PathBuf {
        let dir = issue_dir(root, "EN-1", clone, name).await;
        worktree::ensure(clone, &dir, branch, "main", &[])
            .await
            .expect("worktree")
    }

    #[tokio::test]
    async fn sessions_of_one_issue_in_two_repositories_get_a_worktree_each() {
        let tmp = std::env::temp_dir().join(format!("mothership-issue-dir-{}", std::process::id()));
        let (app, _, app_test) = clones(&tmp);
        let root = app.parent().expect("temp dir").join("worktrees");
        let first = cut(&root, &app_test, "app-test", "en-1").await;
        assert_eq!(first, root.join("EN-1"));
        let second = cut(&root, &app, "app", "en-1").await;
        assert_eq!(second, root.join("EN-1-app"));
        assert_eq!(
            worktree::main_clone(&second).await.ok(),
            app.canonicalize().ok()
        );
        assert_eq!(cut(&root, &app_test, "app-test", "en-1").await, first);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn a_worktree_of_the_same_repository_is_reused() {
        let tmp = std::env::temp_dir().join(format!("mothership-same-repo-{}", std::process::id()));
        let (app, app_2, _) = clones(&tmp);
        let root = app.parent().expect("temp dir").join("worktrees");
        let first = cut(&root, &app, "app", "en-1").await;
        assert_eq!(first, root.join("EN-1"));
        assert_eq!(cut(&root, &app, "app", "en-1").await, first, "same clone");
        assert_eq!(
            cut(&root, &app_2, "app", "en-1-again").await,
            first,
            "another clone of the same origin"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_pull_request_session_has_a_directory_of_its_own() {
        let mut rec = SessionRec {
            identifier: "SH-7".into(),
            ..SessionRec::default()
        };
        assert_eq!(dir_name(&rec), "SH-7");
        rec.pull_request = Some(crate::store::PullRequestRef {
            number: 12,
            ..Default::default()
        });
        assert_eq!(dir_name(&rec), "SH-7-pr12");
    }

    #[test]
    fn a_lost_worktree_is_cut_again_on_its_branch() {
        let mut rec = SessionRec::default();
        assert_eq!(recorded_branch(&rec), None, "first turn: ask Linear");
        rec.branch = Some(String::new());
        assert_eq!(recorded_branch(&rec), None);
        rec.branch = Some("alice/sh-7".into());
        rec.workspace = Some(PathBuf::from("/gone/SH-7"));
        assert_eq!(recorded_branch(&rec), Some("alice/sh-7"));
    }
}
