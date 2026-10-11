//! What `repos.json` cannot say about itself: each entry's directory, clone and files on disk.

use super::Repo;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

/// What `parse` cannot see: the directory exists, is the top of a git repository when it says
/// so, and every MCP config file is there. Returns the directory with symlinks resolved.
pub(super) fn check_on_disk(repo: &Repo) -> Result<PathBuf> {
    let name = &repo.name;
    if !repo.path.is_dir() {
        bail!(
            "repo {name:?} path {} is not a directory",
            repo.path.display()
        );
    }
    let resolved = std::fs::canonicalize(&repo.path)
        .with_context(|| format!("repo {name:?} path {}", repo.path.display()))?;
    if repo.git {
        let mut git = std::process::Command::new("git");
        // git's message goes into the error; keep it in English whatever the host locale.
        git.env("LC_ALL", "C");
        // A pre-commit hook exports these; they would answer for another repository.
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
        ] {
            git.env_remove(var);
        }
        let top = git
            .arg("-C")
            .arg(&repo.path)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("running git")?;
        // A subdirectory of a clone, or anything under a `$HOME` kept in git, is inside a
        // repository too, but worktrees are cut from the clone's top.
        // git's own complaint (not a repository, `safe.directory`) is the useful part when
        // it fails.
        let why = String::from_utf8_lossy(&top.stderr).trim().to_string();
        let top = String::from_utf8_lossy(&top.stdout);
        let top = top.trim_end_matches('\n');
        if top.is_empty() || std::fs::canonicalize(top).ok().as_ref() != Some(&resolved) {
            bail!(
                "repo {name:?} path {} is not the top of a git repository; point it at the \
                 clone's top directory, or set \"git\": false to work in it directly{}",
                repo.path.display(),
                if why.is_empty() {
                    String::new()
                } else {
                    format!(" (git: {why})")
                }
            );
        }
    }
    if let Some(missing) = repo.mcp_configs.iter().find(|f| !f.is_file()) {
        bail!(
            "repo {name:?} mcp_configs file {} does not exist",
            missing.display()
        );
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn git_init(dir: &Path) {
        let ok = std::process::Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .args(["init", "-q"])
            .arg(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git init");
    }

    #[test]
    fn git_repos_must_be_the_top_of_a_clone() {
        let dir = std::env::temp_dir().join(format!("mothership-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        git_init(&dir.join("clone"));
        std::fs::create_dir(dir.join("clone/sub")).unwrap();
        let check = |path: PathBuf| {
            check_on_disk(&Repo::single(path, "main".into())).map_err(|e| format!("{e:#}"))
        };
        let expected = std::fs::canonicalize(dir.join("clone")).unwrap();
        let top = check(dir.join("clone"));
        let sub = check(dir.join("clone/sub"));
        let plain = check(dir.clone());
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(top.unwrap(), expected, "the resolved clone");
        let sub = sub.unwrap_err();
        assert!(
            sub.contains("is not the top of a git repository") && !sub.contains("(git:"),
            "git succeeds here, it just names another top: {sub}"
        );
        let plain = plain.unwrap_err();
        assert!(
            plain.contains("not the top") && plain.contains("(git: fatal: not a git repository"),
            "outside any clone, git says why: {plain}"
        );
    }
}
