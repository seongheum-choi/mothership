//! Git worktrees, one per Linear issue, cut from the main clone.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Returns the issue's worktree, creating it under `dir` with `branch` if needed.
///
/// An existing `dir` is reused as is. An existing checkout of the branch (an earlier session, another tool's worktree) is
/// reused, because git refuses a second one. A branch that exists locally or on origin is
/// continued. A new branch starts from `origin/<base>` with no upstream, so a bare
/// `git push` can never target the base branch.
pub async fn ensure(repo: &Path, dir: &Path, branch: &str, base: &str) -> Result<PathBuf> {
    if let Err(e) = git(repo, &["fetch", "origin"]).await {
        tracing::warn!("git fetch failed, using local refs: {e:#}");
    }
    let porcelain = git(repo, &["worktree", "list", "--porcelain"]).await?;
    if let Some(existing) = checked_out_at(&porcelain, branch) {
        return Ok(existing);
    }
    if dir.exists() {
        // The issue's own worktree. Reuse it on whatever branch it is now: skills move it
        // across stacked branches, and it may belong to another clone of the same repository.
        if git(dir, &["rev-parse", "--is-inside-work-tree"])
            .await
            .is_ok()
        {
            return Ok(dir.to_path_buf());
        }
        bail!("{} exists but is not a git worktree", dir.display());
    }
    let path = dir.to_str().context("worktree path is not UTF-8")?;
    if has_ref(repo, &format!("refs/heads/{branch}")).await {
        git(repo, &["worktree", "add", path, branch]).await?;
    } else if has_ref(repo, &format!("refs/remotes/origin/{branch}")).await {
        git(
            repo,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                path,
                &format!("origin/{branch}"),
            ],
        )
        .await?;
    } else {
        git(
            repo,
            &[
                "worktree",
                "add",
                "--no-track",
                "-b",
                branch,
                path,
                &format!("origin/{base}"),
            ],
        )
        .await?;
    }
    Ok(dir.to_path_buf())
}

/// The branch checked out in `worktree`, if it is on one.
pub async fn current_branch(worktree: &Path) -> Option<String> {
    let head = git(worktree, &["symbolic-ref", "--short", "HEAD"])
        .await
        .ok()?;
    Some(head.trim().to_string()).filter(|b| !b.is_empty())
}

/// The main clone `worktree` was cut from, from its shared git directory.
pub async fn main_clone(worktree: &Path) -> Result<PathBuf> {
    let common = git(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    Ok(clone_of(Path::new(common.trim())))
}

/// `<clone>/.git` belongs to `<clone>`; a bare repository is its own git directory.
fn clone_of(common_dir: &Path) -> PathBuf {
    match (common_dir.file_name(), common_dir.parent()) {
        (Some(name), Some(parent)) if name == ".git" => parent.to_path_buf(),
        _ => common_dir.to_path_buf(),
    }
}

async fn has_ref(repo: &Path, name: &str) -> bool {
    git(repo, &["rev-parse", "--verify", "--quiet", name])
        .await
        .is_ok()
}

/// Path of the worktree that has `branch` checked out, from `git worktree list --porcelain`.
fn checked_out_at(porcelain: &str, branch: &str) -> Option<PathBuf> {
    let want = format!("branch refs/heads/{branch}");
    porcelain.split("\n\n").find_map(|block| {
        let path = block.lines().find_map(|l| l.strip_prefix("worktree "))?;
        block
            .lines()
            .any(|l| l == want)
            .then(|| PathBuf::from(path))
    })
}

async fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_existing_checkout() {
        let porcelain = "worktree /repo\nHEAD abc\nbranch refs/heads/main\n\n\
                         worktree /wt/A-1\nHEAD def\nbranch refs/heads/dev/a-1\n";
        assert_eq!(
            checked_out_at(porcelain, "dev/a-1"),
            Some(PathBuf::from("/wt/A-1"))
        );
        assert_eq!(checked_out_at(porcelain, "dev/a-10"), None);
    }

    #[test]
    fn clone_of_common_dir() {
        assert_eq!(
            clone_of(Path::new("/src/app/.git")),
            PathBuf::from("/src/app")
        );
        assert_eq!(
            clone_of(Path::new("/src/app.git")),
            PathBuf::from("/src/app.git")
        );
    }
}
