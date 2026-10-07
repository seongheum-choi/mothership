//! The repositories one mothership works on (`<home>/repos.json`), and which one a Linear
//! issue belongs to.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
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
fn parse(text: &str, user_home: &Path) -> Result<(Vec<Repo>, Vec<Option<PathBuf>>)> {
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

/// A Linear entity an issue refers to: shown by `name`, matched by any of `name` and
/// `aliases` (id, slug, team key).
#[derive(Default, Clone)]
pub struct Ref {
    pub name: String,
    pub aliases: Vec<String>,
}

impl Ref {
    fn matches(&self, configured: &str) -> bool {
        same(&self.name, configured) || self.aliases.iter().any(|a| same(a, configured))
    }
}

/// What an issue says about where it belongs.
#[derive(Default)]
pub struct IssueFacts {
    /// Text searched for `[repo=<name>]`: the issue description and the prompt.
    pub texts: Vec<String>,
    pub labels: Vec<Ref>,
    pub project: Option<Ref>,
    pub team: Option<Ref>,
    /// Labels, project and team could not be looked up, so they are missing, not absent.
    pub lookup_failed: bool,
}

/// A selected repository and the rule that selected it, for the session's first thought.
#[derive(Debug, PartialEq)]
pub struct Choice<'a> {
    pub repo: &'a Repo,
    pub reason: String,
}

/// Picks the repository for an issue: a `[repo=<name>]` directive (the first text that has
/// one wins), then labels, then the project, then the team. With a single repository there is
/// nothing to choose, and the issue is not read at all. The error is a question for the
/// person who started the session.
pub fn select<'a>(repos: &'a [Repo], facts: &IssueFacts) -> Result<Choice<'a>, String> {
    if let [repo] = repos {
        return Ok(Choice {
            repo,
            reason: "the only configured repository".into(),
        });
    }
    let reply = "Reply with `[repo=<name>]` to choose one.";
    if let Some(name) = facts.texts.iter().find_map(|t| directive(t)) {
        return by_name(repos, name)
            .map(|repo| Choice {
                repo,
                reason: format!("`[repo={name}]` in the issue"),
            })
            .ok_or_else(|| {
                format!(
                    "`[repo={name}]` names no repository I know. {reply} {}",
                    known(repos)
                )
            });
    }
    // A `repo:<name>` label names the repository directly, so an unknown name is a mistake to
    // ask about, not a label to pass over.
    let mut label_hits = Vec::new();
    for label in &facts.labels {
        let hits: Vec<&Repo> = match strip_prefix_ignore_case(&label.name, "repo:") {
            Some(name) => {
                let repo = by_name(repos, name).ok_or_else(|| {
                    format!(
                        "The label `{}` names no repository I know. {reply} {}",
                        label.name,
                        known(repos)
                    )
                })?;
                vec![repo]
            }
            None => repos
                .iter()
                .filter(|r| r.labels.iter().any(|l| label.matches(l)))
                .collect(),
        };
        label_hits.extend(
            hits.into_iter()
                .map(|r| (r, format!("label `{}`", label.name))),
        );
    }
    let levels: [(&str, Vec<(&Repo, String)>); 3] = [
        ("labels", label_hits),
        (
            "project",
            claimed(repos, facts.project.as_ref(), "project", |r| {
                &r.linear_projects
            }),
        ),
        (
            "team",
            claimed(repos, facts.team.as_ref(), "team", |r| &r.linear_teams),
        ),
    ];
    for (what, mut hits) in levels {
        // Stable sort keeps the first reason found for each repository.
        hits.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        hits.dedup_by(|a, b| a.0.name == b.0.name);
        match hits.as_slice() {
            [] => {}
            [(repo, reason)] => {
                return Ok(Choice {
                    repo,
                    reason: reason.clone(),
                });
            }
            many => {
                let names: Vec<&str> = many.iter().map(|(r, _)| r.name.as_str()).collect();
                return Err(format!(
                    "The issue's {what} point to more than one repository ({}). Keep one \
                     `repo:<name>` label, or put `[repo=<name>]` in the description or your reply.",
                    names.join(", ")
                ));
            }
        }
    }
    if facts.lookup_failed {
        return Err(format!(
            "I couldn't look up this issue's labels, project and team in Linear, so I can't \
             tell which repository it is for. {reply} {}",
            known(repos)
        ));
    }
    Err(format!(
        "I can't tell which repository this issue is for. Add a `repo:<name>` label, or put \
         `[repo=<name>]` in the description or your reply. {}",
        known(repos)
    ))
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

fn claimed<'a>(
    repos: &'a [Repo],
    entity: Option<&Ref>,
    kind: &str,
    field: impl Fn(&Repo) -> &Vec<String>,
) -> Vec<(&'a Repo, String)> {
    let Some(entity) = entity else {
        return Vec::new();
    };
    repos
        .iter()
        .filter(|r| field(r).iter().any(|c| entity.matches(c)))
        .map(|r| (r, format!("{kind} `{}`", entity.name)))
        .collect()
}

pub fn by_name<'a>(repos: &'a [Repo], name: &str) -> Option<&'a Repo> {
    repos.iter().find(|r| same(&r.name, name))
}

fn known(repos: &[Repo]) -> String {
    let names: Vec<String> = repos.iter().map(|r| format!("`{}`", r.name)).collect();
    format!("Repositories: {}.", names.join(", "))
}

/// The name in the first `[repo=<name>]` of `text` that could be a repository name, so a
/// placeholder like `[repo=<name>]` in prose is passed over.
fn directive(text: &str) -> Option<&str> {
    text.match_indices("[repo=").find_map(|(i, tag)| {
        let rest = &text[i + tag.len()..];
        let name = rest[..rest.find(']')?].trim();
        valid_name(name).then_some(name)
    })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| text[prefix.len()..].trim())
}

fn same(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/Users/me")
    }

    fn repos(json: &str) -> Vec<Repo> {
        parse(json, &home()).unwrap().0
    }

    fn two() -> Vec<Repo> {
        repos(
            r#"[
              {"name": "app", "path": "/src/app", "labels": ["backend"],
               "linear_teams": ["APP"], "linear_projects": ["launch"]},
              {"name": "vault", "path": "~/vault", "git": false,
               "labels": ["notes", "label-id"],
               "linear_teams": ["Personal"]}
            ]"#,
        )
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

    #[test]
    fn single_repo_from_legacy_settings() {
        let repo = Repo::single(PathBuf::from("/src/mothership"), "dev".into());
        assert_eq!(repo.name, "mothership");
        assert_eq!(repo.base_branch, "dev");
        assert!(repo.git);
        // An issue about routing may well mention another repository; one repository means
        // there is nothing to route, as before repos.json.
        let facts = IssueFacts {
            texts: vec!["Explain `[repo=other]`".into()],
            labels: labels(&["repo:other"]),
            lookup_failed: true,
            ..IssueFacts::default()
        };
        let choice = select(std::slice::from_ref(&repo), &facts).unwrap();
        assert_eq!(choice.repo, &repo);
        assert_eq!(choice.reason, "the only configured repository");
    }

    #[test]
    fn finds_directive() {
        assert_eq!(directive("Fix it.\n[repo= vault ]"), Some("vault"));
        assert_eq!(directive("[repo=]"), None);
        assert_eq!(directive("add `[repo=<name>]` or `[repo=…]`"), None);
        assert_eq!(directive("`[repo=<name>]`, here: [repo=app]"), Some("app"));
        assert_eq!(directive("[repo=open"), None);
        assert_eq!(directive("no directive"), None);
    }

    fn entity(name: &str, aliases: &[&str]) -> Ref {
        Ref {
            name: name.into(),
            aliases: aliases.iter().map(ToString::to_string).collect(),
        }
    }

    fn labels(names: &[&str]) -> Vec<Ref> {
        names.iter().map(|n| entity(n, &[])).collect()
    }

    #[test]
    fn selects_in_priority_order() {
        let repos = two();
        let pick =
            |facts: IssueFacts| select(&repos, &facts).map(|c| (c.repo.name.as_str(), c.reason));
        let launch = || Some(entity("Launch v2", &["prj-id", "launch"]));
        let personal = || Some(entity("Personal", &["team-id", "PER"]));
        // Directive beats labels, labels beat project, project beats team.
        assert_eq!(
            pick(IssueFacts {
                texts: vec!["Fix it. [repo=vault]".into()],
                labels: labels(&["backend"]),
                ..IssueFacts::default()
            }),
            Ok(("vault", "`[repo=vault]` in the issue".into()))
        );
        assert_eq!(
            pick(IssueFacts {
                labels: labels(&["bug", "Notes"]),
                project: launch(),
                ..IssueFacts::default()
            }),
            Ok(("vault", "label `Notes`".into()))
        );
        assert_eq!(
            pick(IssueFacts {
                labels: labels(&["repo:App"]),
                team: personal(),
                ..IssueFacts::default()
            }),
            Ok(("app", "label `repo:App`".into()))
        );
        assert_eq!(
            pick(IssueFacts {
                labels: vec![entity("Documentation", &["label-id"])],
                ..IssueFacts::default()
            }),
            Ok(("vault", "label `Documentation`".into())),
            "labels match by id"
        );
        assert_eq!(
            pick(IssueFacts {
                project: launch(),
                team: personal(),
                ..IssueFacts::default()
            }),
            Ok(("app", "project `Launch v2`".into())),
            "projects match by slug"
        );
        assert_eq!(
            pick(IssueFacts {
                team: Some(entity("Engineering", &["team-x", "APP"])),
                ..IssueFacts::default()
            }),
            Ok(("app", "team `Engineering`".into())),
            "teams match by key"
        );
        assert_eq!(
            pick(IssueFacts {
                texts: vec!["[repo=app]".into(), "Fix it. [repo=nope]".into()],
                ..IssueFacts::default()
            }),
            Ok(("app", "`[repo=app]` in the issue".into())),
            "a reply corrects the description"
        );
        // Two labels for the same repository are not a conflict.
        assert_eq!(
            pick(IssueFacts {
                labels: labels(&["backend", "repo:app"]),
                ..IssueFacts::default()
            }),
            Ok(("app", "label `backend`".into()))
        );
    }

    #[test]
    fn asks_when_it_cannot_choose() {
        let repos = two();
        let ask = |facts: IssueFacts| select(&repos, &facts).unwrap_err();
        assert!(ask(IssueFacts::default()).contains("`repo:<name>` label"));
        assert!(
            ask(IssueFacts {
                texts: vec!["[repo=nope]".into()],
                ..IssueFacts::default()
            })
            .contains("`[repo=nope]` names no repository I know. Reply with `[repo=<name>]`")
        );
        assert!(
            ask(IssueFacts {
                labels: labels(&["repo:nope", "backend"]),
                team: Some(entity("APP", &[])),
                ..IssueFacts::default()
            })
            .contains("The label `repo:nope` names no repository"),
            "an unknown repo: label does not fall through to the team"
        );
        let failed = ask(IssueFacts {
            lookup_failed: true,
            ..IssueFacts::default()
        });
        assert!(failed.contains("couldn't look up") && failed.contains("`[repo=<name>]`"));
        assert!(!failed.contains("label,"), "{failed}");
        assert!(
            ask(IssueFacts {
                labels: labels(&["backend", "notes"]),
                ..IssueFacts::default()
            })
            .contains("more than one repository (app, vault)")
        );
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
