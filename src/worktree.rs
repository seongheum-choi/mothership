//! Git worktrees, one per Linear issue, cut from the main clone.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Returns the issue's worktree, creating it under `dir` with `branch` if needed.
///
/// An existing `dir` is reused as is. An existing checkout of the branch (an earlier session, another tool's worktree) is
/// reused, because git refuses a second one. A branch that exists locally or on origin is
/// continued. A new branch starts from `origin/<base>` with no upstream, so a bare
/// `git push` can never target the base branch. The fetch from origin runs with `fetch_env`
/// added, such as the credentials of [`crate::gerrit::git_env`].
pub async fn ensure(
    repo: &Path,
    dir: &Path,
    branch: &str,
    base: &str,
    fetch_env: &[(String, String)],
) -> Result<PathBuf> {
    if let Err(e) = git_with(repo, &["fetch", "origin"], fetch_env).await {
        tracing::warn!(
            "git fetch in {} failed, using local refs: {e:#}",
            repo.display()
        );
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
    if has_branch(repo, branch).await {
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

/// Removes the worktree at `dir` from the main clone `repo` and keeps its branch. Returns
/// `false`, removing nothing, when it has uncommitted changes or untracked files.
pub async fn remove(repo: &Path, dir: &Path) -> Result<bool> {
    if !git(dir, &["status", "--porcelain"])
        .await?
        .trim()
        .is_empty()
    {
        return Ok(false);
    }
    let path = dir.to_str().context("worktree path is not UTF-8")?;
    git(repo, &["worktree", "remove", path]).await?;
    Ok(true)
}

/// Whether the clone at `repo` has the local branch `branch`.
pub async fn has_branch(repo: &Path, branch: &str) -> bool {
    has_ref(repo, &format!("refs/heads/{branch}")).await
}

async fn has_ref(repo: &Path, name: &str) -> bool {
    git(repo, &["rev-parse", "--verify", "--quiet", name])
        .await
        .is_ok()
}

/// The worktree of the clone at `repo` that has `branch` checked out.
pub async fn checkout_of(repo: &Path, branch: &str) -> Option<PathBuf> {
    let porcelain = git(repo, &["worktree", "list", "--porcelain"]).await.ok()?;
    checked_out_at(&porcelain, branch)
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
    git_with(repo, args, &[]).await
}

async fn git_with(repo: &Path, args: &[&str], env: &[(String, String)]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .envs(env.iter().cloned())
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

    /// Serves the bare repository `root` over git's dumb HTTP protocol to clients that send
    /// `acme-bot:secret` as Basic credentials, and answers everyone else 401, as Gerrit's `/a/`
    /// URLs do. Returns its URL.
    fn authenticated_origin(root: PathBuf) -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding a port");
        let url = format!("http://{}", listener.local_addr().expect("local address"));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(&stream);
                let mut lines = Vec::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    lines.push(line.trim_end().to_string());
                }
                let path = lines
                    .first()
                    .and_then(|l| l.split(' ').nth(1))
                    .unwrap_or("/")
                    .split('?')
                    .next()
                    .unwrap_or("/")
                    .to_string();
                let authorized = lines
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case("authorization: Basic YWNtZS1ib3Q6c2VjcmV0"));
                let (status, body) = if !authorized {
                    (
                        "401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"",
                        Vec::new(),
                    )
                } else if let Ok(body) = std::fs::read(root.join(path.trim_start_matches('/'))) {
                    ("200 OK", body)
                } else {
                    ("404 Not Found", Vec::new())
                };
                let mut out = &stream;
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = out.write_all(head.as_bytes());
                let _ = out.write_all(&body);
            }
        });
        url
    }

    fn run(dir: &Path, args: &[&str]) -> String {
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
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn the_fetch_signs_in_to_a_gerrit_origin() {
        let tmp = std::env::temp_dir().join(format!("mothership-fetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("temp dir");
        let (src, origin, clone) = (tmp.join("src"), tmp.join("origin.git"), tmp.join("clone"));
        run(&tmp, &["init", "-q", "-b", "main", "src"]);
        run(&src, &["commit", "-q", "--allow-empty", "-m", "first"]);
        run(&tmp, &["clone", "-q", "--bare", "src", "origin.git"]);
        run(&tmp, &["clone", "-q", "origin.git", "clone"]);
        let url = authenticated_origin(origin.clone());
        run(
            &clone,
            &["remote", "set-url", "origin", &format!("{url}/a/app")],
        );
        // The server maps `/a/app/...` onto the bare repository's files.
        std::fs::create_dir_all(origin.join("a")).expect("dir");
        std::os::unix::fs::symlink(&origin, origin.join("a/app")).expect("symlink");
        run(&src, &["commit", "-q", "--allow-empty", "-m", "second"]);
        run(
            &src,
            &["push", "-q", origin.to_str().expect("utf-8"), "main"],
        );
        run(&origin, &["update-server-info"]);
        let second = run(&src, &["rev-parse", "HEAD"]);

        // No global helper, no prompt, no proxy for the loopback origin.
        let base: Vec<(String, String)> = [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_ASKPASS", ""),
            ("SSH_ASKPASS", ""),
            ("no_proxy", "*"),
            ("NO_PROXY", "*"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
        let stale = ensure(&clone, &tmp.join("A-1"), "a-1", "main", &base)
            .await
            .expect("cut from local refs");
        assert_ne!(
            run(&stale, &["rev-parse", "HEAD"]),
            second,
            "no credentials"
        );

        let gerrit = crate::config::GerritConfig {
            url: url.clone(),
            username: "acme-bot".into(),
            http_password: "secret".into(),
            webhook_token: None,
            trusted_emails: Vec::new(),
            mention: "acme-bot".into(),
        };
        let mut env = base.clone();
        env.extend(crate::gerrit::git_env(Some(&gerrit)));
        let fresh = ensure(&clone, &tmp.join("A-2"), "a-2", "main", &env)
            .await
            .expect("cut from origin");
        assert_eq!(run(&fresh, &["rev-parse", "HEAD"]), second);
        let config = std::fs::read_to_string(clone.join(".git/config")).expect("config");
        assert!(!config.contains("secret"), "{config}");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
