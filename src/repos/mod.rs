//! The repositories one mothership works on (`<home>/repos.json`), and which one a Linear
//! issue belongs to.

mod load;
mod select;

pub use load::load;
pub use select::{IssueFacts, Ref, select};

use select::same;
use std::path::{Path, PathBuf};

/// Told to sessions in a repository that is not under git, instead of the review backend's
/// instructions.
const NO_VCS_INSTRUCTIONS: &str = "This directory is not a git repository; it is synced by \
     other means. Edit files in place. Do not run git, make commits, or open pull requests; \
     say what you changed in your final reply.";

#[derive(Debug, PartialEq)]
pub struct Repo {
    /// Named by `repo:<name>` labels and `[repo=<name>]` in issue descriptions.
    pub name: String,
    /// Main clone that issue worktrees are cut from, or for a non-git repository the
    /// directory sessions work in directly.
    pub path: PathBuf,
    pub base_branch: String,
    /// Linear label names or ids that route to this repository.
    pub labels: Vec<String>,
    /// Linear team keys, names or ids that route to this repository.
    pub linear_teams: Vec<String>,
    /// Linear project names, ids or slugs that route to this repository.
    pub linear_projects: Vec<String>,
    /// Contents of `prompt_file`, read at startup.
    pub prompt: Option<String>,
    /// Extra MCP config files for sessions in this repository, next to the global ones.
    pub mcp_configs: Vec<PathBuf>,
    /// False for a plain directory: no worktrees, one turn at a time, no commits.
    pub git: bool,
}

impl Repo {
    /// The repository `REPO_PATH`/`BASE_BRANCH` describe when there is no `repos.json`.
    pub fn single(path: PathBuf, base_branch: String) -> Self {
        let name = path
            .file_name()
            .map_or_else(|| "repo".into(), |n| n.to_string_lossy().into_owned());
        Self {
            name,
            path,
            base_branch,
            labels: Vec::new(),
            linear_teams: Vec::new(),
            linear_projects: Vec::new(),
            prompt: None,
            mcp_configs: Vec::new(),
            git: true,
        }
    }

    /// What an issue session is told to do with finished work. A repository's own prompt
    /// replaces `default` (the global prompt file or review backend); a non-git repository
    /// always gets the no-commit instructions, followed by its own prompt.
    pub fn instructions(&self, default: &str) -> String {
        match (self.git, &self.prompt) {
            (true, Some(prompt)) => prompt.clone(),
            (true, None) => default.to_string(),
            (false, Some(prompt)) => format!("{NO_VCS_INSTRUCTIONS}\n\n{prompt}"),
            (false, None) => NO_VCS_INSTRUCTIONS.to_string(),
        }
    }
}

/// The repository whose main clone is `main_clone`, comparing resolved paths so symlinks and
/// `~` spellings agree.
pub fn owning<'a>(repos: &'a [Repo], main_clone: &Path) -> Option<&'a Repo> {
    let resolved = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let main_clone = resolved(main_clone);
    repos
        .iter()
        .find(|r| r.git && resolved(&r.path) == main_clone)
}

pub fn by_name<'a>(repos: &'a [Repo], name: &str) -> Option<&'a Repo> {
    repos.iter().find(|r| same(&r.name, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn two() -> Vec<Repo> {
        load::parse(
            r#"[
              {"name": "app", "path": "/src/app", "labels": ["backend"],
               "linear_teams": ["APP"], "linear_projects": ["launch"]},
              {"name": "vault", "path": "~/vault", "git": false,
               "labels": ["notes", "label-id"],
               "linear_teams": ["Personal"]}
            ]"#,
            Path::new("/Users/me"),
        )
        .unwrap()
        .0
    }

    #[test]
    fn owning_matches_main_clone_of_git_repos() {
        let repos = two();
        assert_eq!(
            owning(&repos, Path::new("/src/app")).map(|r| r.name.as_str()),
            Some("app")
        );
        assert_eq!(
            owning(&repos, Path::new("/Users/me/vault")),
            None,
            "not git"
        );
        assert_eq!(owning(&repos, Path::new("/src/other")), None);
    }

    #[test]
    fn instructions_depend_on_git_and_prompt() {
        let mut repo = Repo::single(PathBuf::from("/a"), "main".into());
        assert_eq!(repo.instructions("open a PR"), "open a PR");
        repo.prompt = Some("custom".into());
        assert_eq!(repo.instructions("open a PR"), "custom");
        repo.git = false;
        assert_eq!(
            repo.instructions("open a PR"),
            format!("{NO_VCS_INSTRUCTIONS}\n\ncustom")
        );
        repo.prompt = None;
        assert_eq!(repo.instructions("open a PR"), NO_VCS_INSTRUCTIONS);
    }
}
