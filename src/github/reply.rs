//! The `gh-reply` wrapper agents answer on GitHub with, and the GitHub repository each clone's
//! `origin` points at.

use anyhow::{Context, Result, bail};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

/// Hidden marker `gh-reply` appends to every reply. The agent posts as the owner's own
/// account, so neither login nor bot type tells its replies apart from the owner's.
// ponytail: once agents post with a GitHub App user token (SH-198), check `performed_via_github_app` instead.
pub(super) const MARKER: &str = "<!-- mothership -->";

/// Writes `<home>/gh-reply/gh-reply` and returns its directory, which goes first on agents'
/// `PATH`.
pub(super) fn install(home: &Path) -> Result<PathBuf> {
    // A directory of its own: it goes first on agents' PATH, so nothing else may live there.
    let bin_dir = home.join("gh-reply");
    std::fs::create_dir_all(&bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;
    let script = bin_dir.join("gh-reply");
    std::fs::write(&script, reply_script())
        .with_context(|| format!("writing {}", script.display()))?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("making {} executable", script.display()))?;
    Ok(bin_dir)
}

/// `owner/name` of the `origin` remote of the clone at `path`.
pub(super) fn origin_of(path: &Path) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["remote", "get-url", "origin"])
        .output()
        .context("running git remote get-url origin")?;
    if !out.status.success() {
        bail!(
            "git remote get-url origin in {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let url = String::from_utf8_lossy(&out.stdout);
    repo_from_remote(url.trim())
        .with_context(|| format!("origin {:?} is not an owner/name remote", url.trim()))
}

/// `gh-reply <owner/repo> <pr> [<review-comment-id>]`, body on stdin: a threaded reply to a
/// review comment, or else a PR conversation comment, ending in [`MARKER`].
fn reply_script() -> String {
    format!(
        r#"#!/bin/sh
# Installed by mothership. Posts a GitHub pull request reply marked so that mothership does not
# take it for new feedback.
set -eu
if [ $# -lt 2 ] || [ $# -gt 3 ]; then
    echo "usage: gh-reply <owner/repo> <pr-number> [<review-comment-id>] < body" >&2
    exit 2
fi
text="$(cat)"
if [ -z "$(printf '%s' "$text" | tr -d '[:space:]')" ]; then
    echo "gh-reply: empty reply, nothing posted" >&2
    exit 2
fi
body="$text

{MARKER}"
if [ $# -eq 3 ]; then
    exec gh api --silent "repos/$1/pulls/$2/comments/$3/replies" -f body="$body"
fi
exec gh pr comment "$2" --repo "$1" --body "$body"
"#
    )
}

/// `owner/name` from an origin URL: `git@github.com:o/r.git`, `https://github.com/o/r`,
/// `ssh://git@github.com/o/r.git`.
fn repo_from_remote(url: &str) -> Option<String> {
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?.1,
        None => url.split_once(':')?.1,
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    match path.split('/').collect::<Vec<_>>()[..] {
        [owner, name] if !owner.is_empty() && !name.is_empty() => Some(format!("{owner}/{name}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_from_origin_urls() {
        for url in [
            "git@github.com:o/r.git",
            "git@github.com:o/r",
            "https://github.com/o/r.git",
            "https://github.com/o/r/",
            "ssh://git@github.com/o/r.git",
        ] {
            assert_eq!(repo_from_remote(url).as_deref(), Some("o/r"), "{url}");
        }
        assert_eq!(repo_from_remote("/srv/git/r.git"), None);
        assert_eq!(repo_from_remote("https://github.com/o"), None);
        assert_eq!(repo_from_remote("https://example.com/a/b/c"), None);
    }
}
