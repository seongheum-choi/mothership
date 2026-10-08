//! What closing, deleting or unassigning an issue ends: its running sessions, and once closed,
//! its worktrees.

use super::routing::existing_repo;
use crate::{
    app::App,
    repos::{self, Repo},
    store::SessionRec,
    worktree,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

/// A change to an issue that ends the agent's work on it.
#[derive(Debug, PartialEq)]
pub(super) enum IssueChange<'a> {
    /// Moved to a completed or canceled state, or deleted. Holds the issue id.
    Closed(&'a str),
    /// The app was unassigned or undelegated. Holds the issue id.
    Unassigned(&'a str),
}

/// Classifies data-change and app notification webhooks. A state change is recognised by
/// `updatedFrom.stateId`, so later edits to an already closed issue do not count.
/// `issueStatusChanged` notifications are left out: the `Issue` update carries the same change.
/// An empty id is ignored: Zulip sessions record none and must never match.
pub(super) fn issue_change(p: &Value) -> Option<IssueChange<'_>> {
    fn id(v: &Value) -> Option<&str> {
        v.as_str().filter(|id| !id.is_empty())
    }
    let data = &p["data"];
    match (p["type"].as_str()?, p["action"].as_str()?) {
        ("Issue", "remove") => id(&data["id"]).map(IssueChange::Closed),
        ("Issue", "update") => {
            let trashed = data["trashed"] == true;
            let closed = p["updatedFrom"].get("stateId").is_some()
                && matches!(
                    data["state"]["type"].as_str(),
                    Some("completed" | "canceled")
                );
            if trashed || closed {
                id(&data["id"]).map(IssueChange::Closed)
            } else {
                None
            }
        }
        ("AppUserNotification", "issueUnassignedFromYou") => {
            let n = &p["notification"];
            id(&n["issueId"])
                .or_else(|| id(&n["issue"]["id"]))
                .map(IssueChange::Unassigned)
        }
        _ => None,
    }
}

/// Stops the issue's running sessions. A closed issue also marks its sessions closed and
/// removes their worktrees (branches stay); one with uncommitted changes is kept and logged.
pub(super) async fn on_issue_change(app: &Arc<App>, change: IssueChange<'_>) {
    let (issue_id, note) = match change {
        IssueChange::Closed(id) => (id, "The issue was closed, so I stopped working on it."),
        IssueChange::Unassigned(id) => (id, "I was unassigned, so I stopped working on this."),
    };
    let sessions: Vec<(String, SessionRec)> = app.store.read(|s| {
        s.sessions
            .iter()
            .filter(|(_, r)| r.issue_id == issue_id)
            .map(|(k, r)| (k.clone(), r.clone()))
            .collect()
    });
    for (sid, _) in &sessions {
        if app.linear.stop(sid, Some(note.into())) {
            tracing::info!("[{sid}] stopping: {change:?}");
        }
    }
    if matches!(change, IssueChange::Unassigned(_)) {
        return;
    }
    app.store.update(|s| {
        for r in s.sessions.values_mut() {
            if r.issue_id == issue_id {
                r.closed = true;
            }
        }
    });
    let workspaces = app
        .store
        .read(|s| removable_workspaces(&s.sessions, issue_id));
    for (workspace, (sid, recorded)) in workspaces {
        let main_clone = match recorded {
            Some(_) => None,
            None => worktree::main_clone(&workspace).await.ok(),
        };
        let repo = match cleanup_repo(
            &app.cfg.repos,
            &app.cfg.worktrees_dir,
            recorded.as_deref(),
            &workspace,
            main_clone.as_deref(),
        ) {
            Ok(repo) => repo,
            Err(why) => {
                tracing::info!("[{sid}] keeping {}: {why}", workspace.display());
                continue;
            }
        };
        // Waits for a stopped turn to release the worktree. A prompt may have reopened the
        // issue, or another issue's session taken the worktree, in the meantime.
        let _guard = app.lock_workspace(&workspace).await;
        if !app
            .store
            .read(|s| removable_workspaces(&s.sessions, issue_id).contains_key(&workspace))
        {
            tracing::info!(
                "[{sid}] keeping worktree {}: an open session uses it again",
                workspace.display()
            );
            continue;
        }
        if workspace.exists() {
            match worktree::remove(&repo.path, &workspace).await {
                Ok(true) => tracing::info!(
                    "[{sid}] removed worktree {} from {}",
                    workspace.display(),
                    repo.name
                ),
                Ok(false) => {
                    tracing::warn!(
                        "[{sid}] keeping worktree {}: it has uncommitted changes",
                        workspace.display()
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!("[{sid}] removing worktree {}: {e:#}", workspace.display());
                    continue;
                }
            }
        }
        app.store.update(|s| {
            for r in s.sessions.values_mut() {
                if r.issue_id == issue_id && r.workspace.as_ref() == Some(&workspace) {
                    r.workspace = None;
                }
            }
        });
    }
}

/// The closed issue's workspaces that may go, each with the session that recorded it first
/// and its repository. Sessions of one issue share its worktree, but a stacked branch can
/// put another issue's session in it too, so a workspace any open session records stays, and
/// nothing goes while one of the issue's own sessions has been reopened.
fn removable_workspaces(
    sessions: &HashMap<String, SessionRec>,
    issue_id: &str,
) -> BTreeMap<PathBuf, (String, Option<String>)> {
    let mut ours: Vec<_> = sessions
        .iter()
        .filter(|(_, r)| r.issue_id == issue_id)
        .collect();
    if ours.iter().any(|(_, r)| !r.closed) {
        return BTreeMap::new();
    }
    ours.sort_by_key(|(sid, _)| *sid);
    let mut workspaces = BTreeMap::new();
    for (sid, rec) in ours {
        if let Some(w) = &rec.workspace {
            workspaces
                .entry(w.clone())
                .or_insert_with(|| (sid.clone(), rec.repo.clone()));
        }
    }
    workspaces.retain(|w, _| {
        !sessions
            .values()
            .any(|r| !r.closed && r.workspace.as_ref() == Some(w))
    });
    workspaces
}

/// The git repository whose main clone removes the closed issue's `workspace`, or why it
/// stays: the session's recorded repository, else (before routing) the one `main_clone`
/// belongs to. A non-git repository's workspace is the directory itself, so it never goes.
fn cleanup_repo<'a>(
    repos: &'a [Repo],
    worktrees_dir: &Path,
    recorded: Option<&str>,
    workspace: &Path,
    main_clone: Option<&Path>,
) -> Result<&'a Repo, String> {
    let repo = match recorded {
        Some(name) => repos::by_name(repos, name)
            .ok_or_else(|| format!("its repository `{name}` is not configured"))?,
        None => existing_repo(repos, main_clone)
            .ok_or_else(|| "it belongs to no configured repository".to_string())?,
    };
    if !repo.git {
        return Err(format!("`{}` is not a git repository", repo.name));
    }
    // `starts_with` compares components, so `..` could climb out of WORKTREES_DIR.
    if workspace.components().any(|c| c == Component::ParentDir) {
        return Err("its path contains `..`".into());
    }
    if workspace == repo.path || !workspace.starts_with(worktrees_dir) {
        return Err("it is not a worktree under WORKTREES_DIR".into());
    }
    Ok(repo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn closed_issue_worktrees_go_from_their_own_repository() {
        let app = Repo::single("/src/app".into(), "main".into());
        let vault = Repo {
            git: false,
            ..Repo::single("/notes/vault".into(), "main".into())
        };
        let repos = [app, Repo::single("/src/lib".into(), "main".into()), vault];
        let wt = Path::new("/wt");
        let pick = |recorded: Option<&str>, workspace: &str, clone: Option<&str>| {
            cleanup_repo(
                &repos,
                wt,
                recorded,
                Path::new(workspace),
                clone.map(Path::new),
            )
            .map(|r| r.name.clone())
        };
        assert_eq!(pick(Some("lib"), "/wt/SH-1", None), Ok("lib".into()));
        assert_eq!(
            pick(None, "/wt/SH-1", Some("/src/app")),
            Ok("app".into()),
            "before routing: the worktree's main clone"
        );
        assert!(pick(None, "/wt/SH-1", Some("/src/gone")).is_err());
        assert!(pick(Some("gone"), "/wt/SH-1", None).is_err());
        assert!(
            pick(Some("vault"), "/notes/vault", None).is_err(),
            "a non-git repository is the workspace itself"
        );
        assert!(pick(Some("vault"), "/wt/SH-1", None).is_err());
        assert!(pick(Some("app"), "/src/app", None).is_err());
        assert!(pick(Some("app"), "/elsewhere/SH-1", None).is_err());
        assert!(pick(Some("app"), "/wt/../src/app", None).is_err());
    }

    #[test]
    fn closed_issue_keeps_worktrees_open_sessions_use() {
        let rec = |issue: &str, workspace: Option<&str>, closed| SessionRec {
            issue_id: issue.into(),
            workspace: workspace.map(PathBuf::from),
            repo: Some("app".into()),
            closed,
            ..SessionRec::default()
        };
        let sessions = HashMap::from([
            ("a1".to_string(), rec("i9", Some("/wt/SH-9"), true)),
            ("a2".to_string(), rec("i9", Some("/wt/SH-9b"), true)),
            ("b1".to_string(), rec("i10", Some("/wt/SH-9"), false)),
            ("c1".to_string(), rec("i11", Some("/wt/SH-9b"), true)),
            ("chat".to_string(), rec("", None, false)),
        ]);
        let removable = removable_workspaces(&sessions, "i9");
        assert_eq!(
            removable.keys().collect::<Vec<_>>(),
            [Path::new("/wt/SH-9b")],
            "SH-9 stays: i10's open session on a stacked branch records it"
        );
        assert_eq!(removable[Path::new("/wt/SH-9b")].0, "a2");

        let mut reopened = sessions;
        reopened.insert("a3".into(), rec("i9", None, false));
        assert!(
            removable_workspaces(&reopened, "i9").is_empty(),
            "a reopened session of the issue keeps everything"
        );
    }

    #[test]
    fn classifies_issue_changes() {
        let update = |state: &str, updated_from: Value| json!({"type":"Issue","action":"update","data":{"id":"i1","state":{"type":state}},"updatedFrom":updated_from});
        let moved = json!({"stateId":"s0"});
        assert_eq!(
            issue_change(&update("completed", moved.clone())),
            Some(IssueChange::Closed("i1"))
        );
        assert_eq!(
            issue_change(&update("canceled", moved.clone())),
            Some(IssueChange::Closed("i1"))
        );
        assert_eq!(issue_change(&update("started", moved)), None);
        assert_eq!(
            issue_change(&update("completed", json!({"title":"old"}))),
            None,
            "edit to an already closed issue"
        );
        assert_eq!(
            issue_change(
                &json!({"type":"Issue","action":"update","data":{"id":"i1","trashed":true,"state":{"type":"started"}},"updatedFrom":{"trashed":null}})
            ),
            Some(IssueChange::Closed("i1"))
        );
        assert_eq!(
            issue_change(&json!({"type":"Issue","action":"remove","data":{"id":"i1"}})),
            Some(IssueChange::Closed("i1"))
        );
        assert_eq!(
            issue_change(
                &json!({"type":"AppUserNotification","action":"issueUnassignedFromYou","notification":{"issueId":"i1"}})
            ),
            Some(IssueChange::Unassigned("i1"))
        );
        assert_eq!(
            issue_change(
                &json!({"type":"AppUserNotification","action":"issueUnassignedFromYou","notification":{"issue":{"id":"i1"}}})
            ),
            Some(IssueChange::Unassigned("i1"))
        );
        for ignored in [
            json!({"type":"AppUserNotification","action":"issueStatusChanged","notification":{"issueId":"i1"}}),
            json!({"type":"AppUserNotification","action":"issueNewComment","notification":{"issueId":"i1"}}),
            json!({"type":"Issue","action":"create","data":{"id":"i1"}}),
            json!({"type":"Issue","action":"remove","data":{"id":""}}),
            json!({"type":"AppUserNotification","action":"issueUnassignedFromYou","notification":{"issueId":"","issue":{"id":""}}}),
            json!({"type":"AgentSessionEvent","action":"created"}),
        ] {
            assert_eq!(issue_change(&ignored), None, "{ignored}");
        }
    }
}
