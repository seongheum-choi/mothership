//! Reading and validating `<home>/repos.json`.

use super::Repo;
use super::select::{same, valid_name};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// One `repos.json` entry as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    path: String,
    base_branch: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    linear_teams: Vec<String>,
    #[serde(default)]
    linear_projects: Vec<String>,
    prompt_file: Option<String>,
    #[serde(default)]
    mcp_configs: Vec<String>,
    #[serde(default = "default_git")]
    git: bool,
}

fn default_git() -> bool {
    true
}

/// Reads `path`, or returns `None` when it does not exist.
pub fn load(path: &Path, user_home: &Path) -> Result<Option<Vec<Repo>>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let (mut repos, prompt_files) =
        parse(&text, user_home).with_context(|| format!("in {}", path.display()))?;
    for (repo, prompt_file) in repos.iter_mut().zip(prompt_files) {
        check_on_disk(repo).with_context(|| format!("in {}", path.display()))?;
        if let Some(file) = prompt_file {
            repo.prompt = Some(std::fs::read_to_string(&file).with_context(|| {
                format!(
                    "in {}: repo {:?} prompt_file {}",
                    path.display(),
                    repo.name,
                    file.display()
                )
            })?);
        }
    }
    Ok(Some(repos))
}

/// What `parse` cannot see: the directory exists, is a git repository when it says so, and
/// every MCP config file is there.
fn check_on_disk(repo: &Repo) -> Result<()> {
    let name = &repo.name;
    if !repo.path.is_dir() {
        bail!(
            "repo {name:?} path {} is not a directory",
            repo.path.display()
        );
    }
    if repo.git {
        let inside = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo.path)
            .args(["rev-parse", "--git-dir"])
            .output()
            .context("running git")?;
        if !inside.status.success() {
            bail!(
                "repo {name:?} path {} is not a git repository; set \"git\": false to work \
                 in it directly",
                repo.path.display()
            );
        }
    }
    if let Some(missing) = repo.mcp_configs.iter().find(|f| !f.is_file()) {
        bail!(
            "repo {name:?} mcp_configs file {} does not exist",
            missing.display()
        );
    }
    Ok(())
}

/// Parses and validates `repos.json`: a non-empty array of entries with unique names, paths
/// that neither repeat nor nest, no `base_branch` on a non-git entry, and no label, team or
/// project claimed by two repositories. Also returns each entry's resolved
/// `prompt_file`, which the caller reads.
pub(super) fn parse(text: &str, user_home: &Path) -> Result<(Vec<Repo>, Vec<Option<PathBuf>>)> {
    let entries: Vec<Entry> = serde_json::from_str(text)?;
    if entries.is_empty() {
        bail!("no repositories listed");
    }
    let mut repos: Vec<Repo> = Vec::new();
    let mut prompt_files = Vec::new();
    for entry in entries {
        let name = entry.name.trim().to_string();
        if !valid_name(&name) {
            bail!("repo name {name:?} must be letters, digits, '-', '_' or '.'");
        }
        if repos.iter().any(|r| same(&r.name, &name)) {
            bail!("repo name {name:?} is listed twice");
        }
        let resolve = |field: &str, p: &str| {
            expand(p, user_home).with_context(|| format!("repo {name:?} {field}"))
        };
        if !entry.git && entry.base_branch.is_some() {
            bail!("repo {name:?} has a base_branch but \"git\": false; remove one of them");
        }
        let path = resolve("path", &entry.path)?;
        if let Some(other) = repos
            .iter()
            .find(|r| r.path.starts_with(&path) || path.starts_with(&r.path))
        {
            bail!(
                "repo {name:?} path {} overlaps repo {:?} path {}",
                path.display(),
                other.name,
                other.path.display()
            );
        }
        prompt_files.push(
            entry
                .prompt_file
                .as_deref()
                .map(|p| resolve("prompt_file", p))
                .transpose()?,
        );
        repos.push(Repo {
            path,
            base_branch: entry.base_branch.unwrap_or_else(|| "main".into()),
            labels: entry.labels,
            linear_teams: entry.linear_teams,
            linear_projects: entry.linear_projects,
            prompt: None,
            mcp_configs: entry
                .mcp_configs
                .iter()
                .map(|p| resolve("mcp_configs", p))
                .collect::<Result<_>>()?,
            git: entry.git,
            name,
        });
    }
    for (kind, field) in [
        ("label", (|r: &Repo| &r.labels) as fn(&Repo) -> &Vec<String>),
        ("Linear team", |r| &r.linear_teams),
        ("Linear project", |r| &r.linear_projects),
    ] {
        for (i, a) in repos.iter().enumerate() {
            for b in &repos[i + 1..] {
                if let Some(v) = field(a)
                    .iter()
                    .find(|v| field(b).iter().any(|w| same(v, w)))
                {
                    bail!(
                        "{kind} {v:?} is claimed by both {:?} and {:?}",
                        a.name,
                        b.name
                    );
                }
            }
        }
    }
    Ok((repos, prompt_files))
}

/// An absolute path, or one starting with `~/`. Rebuilt from its components so trailing and
/// doubled slashes are gone: paths are compared, and spliced into sandbox rules.
fn expand(path: &str, user_home: &Path) -> Result<PathBuf> {
    let path = match path.strip_prefix("~/") {
        Some(rest) => user_home.join(rest),
        None => PathBuf::from(path),
    };
    if !path.is_absolute() {
        bail!("path {} must be absolute or start with ~/", path.display());
    }
    Ok(path.components().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/Users/me")
    }

    #[test]
    fn parses_entries_with_defaults() {
        let (repos, prompts) = parse(
            r#"[{"name": "app", "path": "/src/app", "prompt_file": "~/p.md",
                 "mcp_configs": ["/etc/mcp.json"]},
                {"name": "lib", "path": "/src/lib/", "base_branch": "trunk"},
                {"name": "vault", "path": "~/vault", "git": false}]"#,
            &home(),
        )
        .unwrap();
        assert_eq!(repos[0].base_branch, "main");
        assert!(repos[0].git);
        assert_eq!(repos[0].mcp_configs, [PathBuf::from("/etc/mcp.json")]);
        assert_eq!(repos[1].path, PathBuf::from("/src/lib"));
        assert_eq!(repos[1].base_branch, "trunk");
        assert_eq!(repos[2].path, PathBuf::from("/Users/me/vault"));
        assert!(!repos[2].git);
        assert_eq!(prompts, [Some(PathBuf::from("/Users/me/p.md")), None, None]);
    }

    #[test]
    fn expand_normalizes_slashes() {
        let expand = |p: &str| expand(p, &home()).unwrap();
        assert_eq!(expand("~/vault/"), PathBuf::from("/Users/me/vault"));
        assert_eq!(expand("/a//b/./c/"), PathBuf::from("/a/b/c"));
        assert_eq!(
            expand("/notes/vault/").display().to_string(),
            "/notes/vault",
            "sandbox rules are built from this text"
        );
    }

    #[test]
    fn rejects_invalid_configs() {
        let error = |json: &str| format!("{:#}", parse(json, &home()).unwrap_err());
        assert!(error("[]").contains("no repositories"));
        assert!(error(r#"[{"name": "a"}]"#).contains("path"));
        assert!(error(r#"[{"name": "a", "path": "/a", "lables": []}]"#).contains("unknown field"));
        assert!(error(r#"[{"name": "a b", "path": "/a"}]"#).contains("must be letters"));
        assert!(error(r#"[{"name": "a", "path": "rel"}]"#).contains("must be absolute"));
        assert!(
            error(r#"[{"name": "a", "path": "/a", "prompt_file": "p.md"}]"#)
                .contains(r#"repo "a" prompt_file: path p.md must be absolute"#)
        );
        assert!(
            error(r#"[{"name": "a", "path": "/a", "git": false, "base_branch": "dev"}]"#)
                .contains("base_branch")
        );
        assert!(
            error(r#"[{"name": "a", "path": "/a"}, {"name": "b", "path": "/a/"}]"#)
                .contains(r#"repo "b" path /a overlaps repo "a" path /a"#)
        );
        assert!(
            error(r#"[{"name": "a", "path": "/a/b"}, {"name": "b", "path": "/a"}]"#)
                .contains("overlaps"),
            "a repository inside another"
        );
        assert!(
            parse(
                r#"[{"name": "a", "path": "/src/app"}, {"name": "b", "path": "/src/app2"}]"#,
                &home()
            )
            .is_ok(),
            "a shared name prefix is not nesting"
        );
        assert!(
            error(r#"[{"name": "a", "path": "/a"}, {"name": "A", "path": "/b"}]"#)
                .contains("listed twice")
        );
        assert!(
            error(
                r#"[{"name": "a", "path": "/a", "labels": ["x"]},
                    {"name": "b", "path": "/b", "labels": ["X"]}]"#
            )
            .contains(r#"label "x" is claimed by both "a" and "b""#)
        );
        assert!(
            error(
                r#"[{"name": "a", "path": "/a", "linear_teams": ["T"]},
                    {"name": "b", "path": "/b", "linear_teams": ["T"]}]"#
            )
            .contains("Linear team")
        );
    }
}
