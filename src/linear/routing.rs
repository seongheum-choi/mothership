//! Which repository and mode a session works in, settled once from the issue and the request.

use super::{Linear, activity::thought};
use crate::{
    app::App,
    modes,
    repos::{self, IssueFacts, Ref, Repo},
    sessions,
};
use serde_json::Value;
use std::sync::Arc;

/// Why a session cannot start its turn yet.
pub(super) enum Blocked {
    /// Nothing on the issue decides the repository, or its labels pick several modes; the
    /// question for the requester.
    Ask(String),
    /// Something only mothership's files can fix: the session's repository is not configured,
    /// or a mode file does not parse.
    Stuck(String),
}

/// The issue lookup (`Linear::issue_routing`), made at most once per webhook and shared by
/// repository and mode selection. `None` when it failed.
pub(super) struct IssueLookup<'a> {
    issue_id: &'a str,
    done: bool,
    found: Option<Value>,
}

impl<'a> IssueLookup<'a> {
    pub(super) fn new(issue_id: &'a str) -> Self {
        Self {
            issue_id,
            done: false,
            found: None,
        }
    }

    async fn get(&mut self, linear: &Linear, app: &App, sid: &str) -> Option<&Value> {
        if !self.done {
            self.done = true;
            self.found = linear
                .issue_routing(app, self.issue_id)
                .await
                .inspect_err(|e| tracing::warn!("[{sid}] issue lookup failed: {e:#}"))
                .ok();
        }
        self.found.as_ref()
    }
}

impl Linear {
    /// Settles which repository the session works in, once: later turns keep it.
    pub(super) async fn choose_repo(
        &self,
        app: &Arc<App>,
        sid: &str,
        issue: &Value,
        request: Option<&str>,
        lookup: &mut IssueLookup<'_>,
    ) -> Result<(), Blocked> {
        let (current, workspace) = app.store.read(|s| {
            s.sessions
                .get(sid)
                .map(|r| (r.repo.clone(), r.workspace.clone()))
                .unwrap_or_default()
        });
        if let Some(name) = current {
            // Choosing again would leave the old repository's worktree and Claude session under
            // another repository's prompt and sandbox.
            return match app.cfg.repo(&name) {
                Some(_) => Ok(()),
                None => Err(Blocked::Stuck(format!(
                    "This session works in `{name}`, which is no longer in repos.json. Add it \
                     back and restart mothership to continue."
                ))),
            };
        }
        if let Some(workspace) = workspace {
            // A session from before repository routing: it stays where its worktree is.
            let Some(repo) = sessions::workspace_repo(&app.cfg.repos, &workspace).await else {
                return Err(Blocked::Stuck(format!(
                    "This session's worktree {} belongs to no repository in repos.json. Add \
                     its repository and restart mothership to continue.",
                    workspace.display()
                )));
            };
            self.settle(app, sid, repo, "the repository of this session's worktree")
                .await;
            return Ok(());
        }
        let null = Value::Null;
        let routing = if app.cfg.repos.len() > 1 {
            lookup.get(self, app, sid).await
        } else {
            Some(&null)
        };
        let facts = issue_facts(routing, issue, request);
        let choice = repos::select(&app.cfg.repos, &facts).map_err(Blocked::Ask)?;
        self.settle(app, sid, choice.repo, &choice.reason).await;
        Ok(())
    }

    /// Settles the session's mode from the issue's labels, once, at its start. Until it is
    /// settled no turn runs, so a conflict or a broken mode file never falls back to the default.
    pub(super) async fn choose_mode(
        &self,
        app: &Arc<App>,
        sid: &str,
        lookup: &mut IssueLookup<'_>,
    ) -> Result<(), Blocked> {
        if !app
            .store
            .read(|s| s.sessions.get(sid).is_some_and(|r| r.mode_pending))
        {
            return Ok(());
        }
        let all = modes::load_all(&modes::dir(&app.cfg.home)).map_err(|e| {
            Blocked::Stuck(format!(
                "A mode file is broken: {e:#}. Fix it and reply to start."
            ))
        })?;
        let choice = if all.is_empty() {
            None
        } else {
            let Some(issue) = lookup.get(self, app, sid).await else {
                return Err(Blocked::Stuck(
                    "Could not look up the issue's labels to choose a mode. Reply to try again."
                        .into(),
                ));
            };
            let labels: Vec<String> = routing_facts(issue)
                .labels
                .into_iter()
                .map(|l| l.name)
                .collect();
            modes::select(&all, &labels).map_err(Blocked::Ask)?
        };
        let name = choice.as_ref().map(|c| c.mode.name.clone());
        app.store.update(|s| {
            if let Some(rec) = s.sessions.get_mut(sid) {
                rec.mode.clone_from(&name);
                rec.mode_pending = false;
            }
        });
        if let Some(c) = choice {
            tracing::info!("[{sid}] mode {} (label {})", c.mode.name, c.label);
            let note = format!("Mode: `{}` (label `{}`).", c.mode.name, c.label);
            self.activity(app, sid, thought(&note), false).await;
        }
        Ok(())
    }

    /// Records the session's repository and tells the requester which one and why.
    async fn settle(&self, app: &Arc<App>, sid: &str, repo: &Repo, reason: &str) {
        let name = repo.name.clone();
        tracing::info!("[{sid}] repository {name} ({reason})");
        app.store.update(|s| {
            if let Some(rec) = s.sessions.get_mut(sid) {
                rec.repo = Some(name.clone());
            }
        });
        let note = format!("Working in `{name}` ({reason}).");
        self.activity(app, sid, thought(&note), false).await;
    }
}

/// What routing looks at: the `issue_routing` result (`None` when the lookup failed), with the
/// webhook's description when the lookup has none. The request (the @mention or reply) comes
/// first, so a reply's `[repo=…]` corrects a wrong one in the description.
fn issue_facts(routing: Option<&Value>, issue: &Value, request: Option<&str>) -> IssueFacts {
    let mut facts = routing.map(routing_facts).unwrap_or_default();
    facts.lookup_failed = routing.is_none();
    if facts.texts.is_empty()
        && let Some(description) = issue["description"].as_str()
    {
        facts.texts.push(description.to_string());
    }
    if let Some(request) = request {
        facts.texts.insert(0, request.to_string());
    }
    facts
}

/// Repository routing facts from an `issue_routing` result (`Null` when unavailable).
fn routing_facts(issue: &Value) -> IssueFacts {
    let entity = |v: &Value, aliases: &[&str]| {
        v["name"].as_str().map(|name| Ref {
            name: name.to_string(),
            aliases: aliases
                .iter()
                .filter_map(|k| v[k].as_str().map(String::from))
                .collect(),
        })
    };
    IssueFacts {
        texts: issue["description"]
            .as_str()
            .map(String::from)
            .into_iter()
            .collect(),
        labels: issue["labels"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| entity(l, &["id"]))
            .collect(),
        project: entity(&issue["project"], &["id", "slugId"]),
        team: entity(&issue["team"], &["id", "key"]),
        lookup_failed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn routing_facts_carry_names_and_aliases() {
        let issue = json!({
            "description": "Do it [repo=app]",
            "project": {"id": "p1", "name": "Launch", "slugId": "abc123"},
            "team": {"id": "t1", "key": "EN", "name": "Engineering"},
            "labels": {"nodes": [{"id": "l1", "name": "backend"}]},
        });
        let facts = routing_facts(&issue);
        assert_eq!(facts.texts, ["Do it [repo=app]"]);
        let project = facts.project.unwrap();
        assert_eq!(
            (project.name.as_str(), project.aliases),
            ("Launch", vec!["p1".into(), "abc123".into()])
        );
        let team = facts.team.unwrap();
        assert_eq!(
            (team.name.as_str(), team.aliases),
            ("Engineering", vec!["t1".into(), "EN".into()])
        );
        assert_eq!(facts.labels[0].aliases, ["l1"]);

        let none = routing_facts(&json!({"project": null, "labels": {"nodes": []}}));
        assert!(none.texts.is_empty() && none.labels.is_empty() && none.project.is_none());
        assert!(routing_facts(&Value::Null).team.is_none());
    }

    #[test]
    fn request_is_searched_before_the_description() {
        let issue = json!({"description": "webhook copy"});
        let looked_up = json!({"description": "Fix it [repo=nope]"});
        let facts = issue_facts(Some(&looked_up), &issue, Some("[repo=app]"));
        assert_eq!(facts.texts, ["[repo=app]", "Fix it [repo=nope]"]);
        assert!(!facts.lookup_failed);

        let facts = issue_facts(None, &issue, None);
        assert_eq!(facts.texts, ["webhook copy"]);
        assert!(facts.lookup_failed);
    }
}
