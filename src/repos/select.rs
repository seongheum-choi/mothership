//! Which configured repository a Linear issue belongs to.

use super::{Repo, by_name};

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

fn known(repos: &[Repo]) -> String {
    let names: Vec<String> = repos.iter().map(|r| format!("`{}`", r.name)).collect();
    format!("Repositories: {}.", names.join(", "))
}

fn directive(text: &str) -> Option<&str> {
    crate::directive::first(text, "repo")
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| text[prefix.len()..].trim())
}

pub(super) fn same(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repos::tests::two;
    use std::path::PathBuf;

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
}
