//! Which of this instance's git repositories a Gerrit project is: those whose `origin` is that
//! project on the host in `GERRIT_URL`.

use crate::repos::Repo;
use anyhow::{Context, Result, bail};
use std::path::Path;

/// Repository names (as in `repos.json`) and their Gerrit projects, replaced whole when the
/// repository list is reloaded.
#[derive(Debug, Default)]
pub(super) struct Projects(Vec<(String, String)>);

impl Projects {
    /// The git repositories in `repos` whose origin is on the host of `gerrit_url`.
    pub(super) fn resolve(repos: &[Repo], gerrit_url: &str) -> Self {
        let mut projects = Vec::new();
        for repo in repos.iter().filter(|r| r.git) {
            match origin_url(&repo.path) {
                Ok(url) => {
                    if let Some(project) = project_of(&url, gerrit_url) {
                        projects.push((repo.name.clone(), project));
                    }
                }
                Err(e) => tracing::warn!("gerrit feedback off for repo {}: {e:#}", repo.name),
            }
        }
        Self(projects)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn list(&self) -> String {
        let projects: Vec<&str> = self.0.iter().map(|(_, p)| p.as_str()).collect();
        projects.join(", ")
    }

    /// Names of the repositories whose origin is `project`; usually one.
    pub(super) fn names(&self, project: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(_, p)| p == project)
            .map(|(name, _)| name.as_str())
            .collect()
    }
}

fn origin_url(path: &Path) -> Result<String> {
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
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The project `origin` names when it is on the host of `gerrit_url`: `https://host/a/team/app`,
/// `https://host/team/app.git` and `ssh://user@host:29418/team/app` all name `team/app`. A path
/// `gerrit_url` itself has (`https://host/r`) is left out of the project.
fn project_of(origin: &str, gerrit_url: &str) -> Option<String> {
    let (host, path) = split_remote(origin)?;
    let (gerrit_host, base) = split_remote(gerrit_url)?;
    if !host.eq_ignore_ascii_case(gerrit_host) {
        return None;
    }
    let path = path.trim_matches('/');
    let path = match base.trim_matches('/') {
        "" => path,
        base => path
            .strip_prefix(base)
            .and_then(|p| p.strip_prefix('/'))
            .unwrap_or(path),
    };
    // `/a/` is the authenticated HTTP path; the project name never starts with it.
    let path = path.strip_prefix("a/").unwrap_or(path);
    let project = path.strip_suffix(".git").unwrap_or(path);
    (!project.is_empty()).then(|| project.to_string())
}

/// The host and path of a URL (`scheme://user@host:port/path`) or an scp-like remote
/// (`user@host:path`).
fn split_remote(url: &str) -> Option<(&str, &str)> {
    let (authority, path, has_port) = if let Some((_, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        (authority, path, true)
    } else {
        let (authority, path) = url.split_once(':')?;
        (authority, path, false)
    };
    let host = authority.rsplit('@').next()?;
    let host = if has_port {
        host.split(':').next()?
    } else {
        host
    };
    (!host.is_empty()).then_some((host, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_from_origins_on_the_gerrit_host() {
        let gerrit = "https://gerrit.example.com";
        for origin in [
            "https://gerrit.example.com/a/team/app",
            "https://gerrit.example.com/team/app.git",
            "https://acme-bot@Gerrit.Example.com:8443/team/app/",
            "ssh://acme-bot@gerrit.example.com:29418/team/app",
            "acme-bot@gerrit.example.com:team/app.git",
        ] {
            assert_eq!(
                project_of(origin, gerrit).as_deref(),
                Some("team/app"),
                "{origin}"
            );
        }
        for origin in [
            "https://github.com/team/app",
            "git@github.com:team/app.git",
            "https://gerrit.example.com/",
            "/srv/git/app.git",
        ] {
            assert_eq!(project_of(origin, gerrit), None, "{origin}");
        }
        assert_eq!(
            project_of("https://host/r/a/app", "https://host/r/").as_deref(),
            Some("app"),
            "GERRIT_URL's own path"
        );
        assert_eq!(
            project_of("ssh://host:29418/app", "https://host/r").as_deref(),
            Some("app")
        );
    }

    #[test]
    fn repository_names_by_project() {
        let projects = Projects(vec![
            ("app".into(), "team/app".into()),
            ("app-v2".into(), "team/app".into()),
            ("lib".into(), "team/lib".into()),
        ]);
        assert_eq!(projects.names("team/app"), ["app", "app-v2"]);
        assert!(
            projects.names("Team/App").is_empty(),
            "project names keep case"
        );
        assert_eq!(projects.list(), "team/app, team/app, team/lib");
    }
}
