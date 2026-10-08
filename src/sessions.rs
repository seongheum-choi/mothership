//! Session lookups shared by the surfaces: which repository a pre-routing session works in, and
//! which session owns a branch.

use crate::{
    repos::{self, Repo},
    store::{SessionRec, Store},
    worktree,
};
use std::path::Path;

/// The repository of a session that has a worktree but predates routing: the one whose main
/// clone the worktree was cut from, or with a single repository, that one as before.
pub fn existing_repo<'a>(repos: &'a [Repo], main_clone: Option<&Path>) -> Option<&'a Repo> {
    main_clone
        .and_then(|clone| repos::owning(repos, clone))
        .or(match repos {
            [only] => Some(only),
            _ => None,
        })
}

/// [`existing_repo`] for the main clone `workspace` was cut from.
pub async fn workspace_repo<'a>(repos: &'a [Repo], workspace: &Path) -> Option<&'a Repo> {
    let main_clone = worktree::main_clone(workspace).await.ok();
    existing_repo(repos, main_clone.as_deref())
}

/// Records that `sid` was just prompted, so it wins over older sessions on its branch.
pub fn mark_prompted(store: &Store, sid: &str) {
    store.update(|s| {
        if let Some(rec) = s.sessions.get_mut(sid) {
            rec.prompted_at = crate::store::now_secs();
        }
    });
}

/// The session that owns a branch.
#[derive(Debug, PartialEq)]
pub enum Found {
    Open(String),
    /// Its issue was closed and its worktree cleaned up; feedback is only logged.
    Closed(String),
}

/// The session that owns `branch` in one of `names` (repository names); see [`find`]. A
/// pre-routing session's repository is worked out from its worktree, and recorded for the
/// session found, as Linear does on its next turn; the turn the caller starts needs it.
pub async fn for_branch(
    store: &Store,
    repos: &[Repo],
    branch: &str,
    names: &[&str],
) -> Option<Found> {
    let mut candidates: Vec<(String, SessionRec)> = store.read(|s| {
        s.sessions
            .iter()
            .filter(|(_, rec)| {
                rec.branch
                    .as_deref()
                    .is_some_and(|b| on_branch(b, branch).is_some())
            })
            .map(|(sid, rec)| (sid.clone(), rec.clone()))
            .collect()
    });
    for (_, rec) in &mut candidates {
        if rec.repo.is_none()
            && let Some(workspace) = &rec.workspace
        {
            rec.repo = workspace_repo(repos, workspace)
                .await
                .map(|r| r.name.clone());
        }
    }
    let found = find(
        candidates.iter().map(|(sid, rec)| (sid, rec)),
        branch,
        names,
    )?;
    let Found::Open(sid) = &found else {
        return Some(found);
    };
    if let Some(name) = candidates
        .iter()
        .find(|(s, _)| s == sid)
        .and_then(|(_, rec)| rec.repo.clone())
    {
        store.update(|s| {
            if let Some(rec) = s.sessions.get_mut(sid)
                && rec.repo.is_none()
            {
                rec.repo = Some(name);
            }
        });
    }
    Some(found)
}

/// Whether a session on `branch` owns the PR head `head`: `Some(true)` for the same branch,
/// `Some(false)` for a branch stacked on it (`en-593` owns `en-593-3`).
fn on_branch(branch: &str, head: &str) -> Option<bool> {
    if branch == head {
        return Some(true);
    }
    head.strip_prefix(branch)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .then_some(false)
}

/// The newest session in one of `repos` (names) whose branch owns `head`. An exact match wins
/// over a stacked one, then an open session over a closed one, so feedback on a closed issue's
/// branch never wakes the issue it is stacked on. Branch names repeat across
/// repositories, so a session in another repository, or with none recorded, never matches.
pub fn find<'a>(
    sessions: impl IntoIterator<Item = (&'a String, &'a SessionRec)>,
    head: &str,
    repos: &[&str],
) -> Option<Found> {
    sessions
        .into_iter()
        .filter(|(_, rec)| {
            rec.repo
                .as_deref()
                .is_some_and(|r| repos.iter().any(|n| n.eq_ignore_ascii_case(r)))
        })
        .filter_map(|(sid, rec)| {
            let exact = on_branch(rec.branch.as_deref()?, head)?;
            Some((exact, !rec.closed, rec.prompted_at, sid))
        })
        .max()
        .map(|(_, open, _, sid)| {
            if open {
                Found::Open(sid.clone())
            } else {
                Found::Closed(sid.clone())
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn existing_worktree_keeps_its_repository() {
        let app = Repo::single("/src/app".into(), "main".into());
        let other = Repo::single("/src/other".into(), "main".into());
        let two = [app, other];
        let pick = |repos: &[Repo], clone: Option<&str>| {
            existing_repo(repos, clone.map(Path::new)).map(|r| r.name.clone())
        };
        assert_eq!(pick(&two, Some("/src/other")), Some("other".into()));
        assert_eq!(pick(&two, Some("/src/gone")), None);
        assert_eq!(pick(&two, None), None);
        assert_eq!(
            pick(&two[..1], None),
            Some("app".into()),
            "single repo as before"
        );
        assert_eq!(pick(&two[..1], Some("/src/gone")), Some("app".into()));
    }

    fn open(found: Option<Found>) -> Option<String> {
        match found? {
            Found::Open(sid) => Some(sid),
            Found::Closed(sid) => panic!("{sid} is closed"),
        }
    }

    #[test]
    fn closed_sessions_get_no_feedback() {
        let rec = |closed, prompted_at| SessionRec {
            repo: Some("app".into()),
            branch: Some("en-593".into()),
            prompted_at,
            closed,
            ..SessionRec::default()
        };
        let closed = HashMap::from([("done".to_string(), rec(true, 9))]);
        assert_eq!(
            find(&closed, "en-593", &["app"]),
            Some(Found::Closed("done".into()))
        );
        let reopened = HashMap::from([
            ("done".to_string(), rec(true, 9)),
            ("again".to_string(), rec(false, 1)),
        ]);
        assert_eq!(
            find(&reopened, "en-593", &["app"]),
            Some(Found::Open("again".into())),
            "an open session wins over a newer closed one"
        );
        let stacked = HashMap::from([
            (
                "s12".to_string(),
                SessionRec {
                    branch: Some("en-5-2".into()),
                    ..rec(true, 1)
                },
            ),
            (
                "s13".to_string(),
                SessionRec {
                    branch: Some("en-5".into()),
                    ..rec(false, 9)
                },
            ),
        ]);
        assert_eq!(
            find(&stacked, "en-5-2", &["app"]),
            Some(Found::Closed("s12".into())),
            "a closed exact match wins over an open stacked one"
        );
        assert_eq!(
            find(&stacked, "en-5", &["app"]),
            Some(Found::Open("s13".into()))
        );
    }

    #[test]
    fn finds_the_newest_session_on_the_branch_or_its_stack() {
        let rec = |branch: &str, prompted_at| SessionRec {
            repo: Some("app".into()),
            branch: Some(branch.into()),
            prompted_at,
            ..SessionRec::default()
        };
        let sessions = HashMap::from([
            ("old".to_string(), rec("en-593", 1)),
            ("new".to_string(), rec("en-593", 2)),
            ("stack".to_string(), rec("en-593-3", 0)),
            ("other".to_string(), rec("en-59", 9)),
            ("chat".to_string(), SessionRec::default()),
        ]);
        let find = |head| open(find(&sessions, head, &["app"]));
        assert_eq!(find("en-593").as_deref(), Some("new"));
        assert_eq!(
            find("en-593-3").as_deref(),
            Some("stack"),
            "exact beats stacked"
        );
        assert_eq!(find("en-593-4").as_deref(), Some("new"));
        assert_eq!(find("en-593-x"), None, "suffix must be a number");
        assert_eq!(find("en-593-"), None);
        assert_eq!(find("main"), None);
    }
}
