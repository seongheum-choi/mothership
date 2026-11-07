//! Session lookups shared by the surfaces: which repository a pre-routing session works in, and
//! which session owns a pull request or a Gerrit topic.

use crate::{
    repos::{self, Repo},
    store::{PullRequestRef, SessionRec, Store},
    worktree,
};
use std::path::{Path, PathBuf};

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

/// The git repository whose main clone has the branch `branch`, when exactly one has: a
/// worktree is removed without its branch, so the branch still names the clone it was cut from.
pub async fn branch_repo<'a>(repos: &'a [Repo], branch: &str) -> Option<&'a Repo> {
    let mut found = Vec::new();
    for repo in repos.iter().filter(|r| r.git) {
        if worktree::has_branch(&repo.path, branch).await {
            found.push(repo);
        }
    }
    match found[..] {
        [only] => Some(only),
        _ => None,
    }
}

/// The repository of a session that recorded none, as Linear settles it: the one its worktree
/// was cut from, or once that worktree is gone, the one that has its branch.
async fn pre_routing_repo<'a>(repos: &'a [Repo], rec: &SessionRec) -> Option<&'a Repo> {
    if let Some(workspace) = rec.workspace.as_deref().filter(|w| w.exists()) {
        return workspace_repo(repos, workspace).await;
    }
    let from_branch = match rec.branch.as_deref().filter(|b| !b.is_empty()) {
        Some(branch) => branch_repo(repos, branch).await,
        None => None,
    };
    from_branch.or(existing_repo(repos, None))
}

/// Records that `sid` was just prompted, so it wins over older sessions on its branch.
pub fn mark_prompted(store: &Store, sid: &str) {
    store.update(|s| {
        if let Some(rec) = s.sessions.get_mut(sid) {
            rec.prompted_at = crate::store::now_secs();
        }
    });
}

/// The session that owns a pull request.
#[derive(Debug, PartialEq)]
pub enum Found {
    Open(String),
    /// Its issue was closed and its worktree cleaned up; feedback is only logged.
    Closed(String),
}

impl Found {
    fn new(open: bool, sid: &str) -> Self {
        if open {
            Self::Open(sid.to_string())
        } else {
            Self::Closed(sid.to_string())
        }
    }
}

/// The session that owns the pull request `pr`, whose head is `branch`, in one of `names`
/// (repository names); see [`find`]. A pre-routing session's repository is worked out from its
/// worktree or branch, and recorded for the session found, as Linear does on its next turn; the
/// turn the caller starts needs it.
pub async fn for_pull_request(
    store: &Store,
    repos: &[Repo],
    pr: &PullRequestRef,
    branch: &str,
    names: &[&str],
) -> Option<Found> {
    let checkouts = checkouts_of(repos, names, branch).await;
    let mut candidates: Vec<(String, SessionRec)> = store.read(|s| {
        s.sessions
            .iter()
            .filter(|(_, rec)| owns(rec, pr, branch).is_some() || on_checkout(rec, &checkouts))
            .map(|(sid, rec)| (sid.clone(), rec.clone()))
            .collect()
    });
    for (_, rec) in &mut candidates {
        // A session whose worktree a skill moved onto the head branch (a stacked one) works
        // there now; a session opened for the pull request could only share that worktree.
        if on_checkout(rec, &checkouts) {
            rec.branch = Some(branch.to_string());
        }
        if rec.repo.is_none() {
            rec.repo = pre_routing_repo(repos, rec).await.map(|r| r.name.clone());
        }
    }
    let found = find(
        candidates.iter().map(|(sid, rec)| (sid, rec)),
        pr,
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

/// Where `branch` is checked out in the clones of the repositories in `names`, resolved so
/// they compare with a session's recorded workspace.
async fn checkouts_of(repos: &[Repo], names: &[&str], branch: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for repo in repos
        .iter()
        .filter(|r| r.git && names.iter().any(|n| n.eq_ignore_ascii_case(&r.name)))
    {
        if let Some(dir) = worktree::checkout_of(&repo.path, branch).await {
            found.push(dir.canonicalize().unwrap_or(dir));
        }
    }
    found
}

/// Whether `rec`, a session not opened for a pull request, works in one of `checkouts`.
fn on_checkout(rec: &SessionRec, checkouts: &[PathBuf]) -> bool {
    rec.pull_request.is_none()
        && rec
            .workspace
            .as_deref()
            .and_then(|w| w.canonicalize().ok())
            .is_some_and(|w| checkouts.contains(&w))
}

/// Whether `rec` owns the pull request `pr` with head `branch`: `Some(true)` for the session a
/// mention opened for it, `Some(false)` for a session on its head branch that no other pull
/// request opened. A branch stacked on the session's (`en-593-3` on `en-593`) is not its own
/// unless its worktree is on it now: the first mention there opens a session for that pull
/// request.
fn owns(rec: &SessionRec, pr: &PullRequestRef, branch: &str) -> Option<bool> {
    match &rec.pull_request {
        Some(own) => own.is(pr).then_some(true),
        None => (rec.branch.as_deref() == Some(branch)).then_some(false),
    }
}

/// The newest session in one of `repos` (names) that owns the pull request `pr` with head
/// `head`. The session opened for the pull request wins over one on its branch, then an open
/// session over a closed one. Branch names repeat across repositories, so a session in another
/// repository, or with none recorded, never matches.
pub fn find<'a>(
    sessions: impl IntoIterator<Item = (&'a String, &'a SessionRec)>,
    pr: &PullRequestRef,
    head: &str,
    repos: &[&str],
) -> Option<Found> {
    sessions
        .into_iter()
        .filter(|(_, rec)| in_repos(rec, repos))
        .filter_map(|(sid, rec)| {
            let opened_for = owns(rec, pr, head)?;
            Some((opened_for, !rec.closed, rec.prompted_at, sid))
        })
        .max()
        .map(|(_, open, _, sid)| Found::new(open, sid))
}

/// The newest session on the issue whose key is `topic`, in one of `repos` (names): every
/// layer of a stack is pushed with the issue key as topic, so the issue's newest session hears
/// all of them. An open session wins over a closed one.
pub fn for_topic<'a>(
    sessions: impl IntoIterator<Item = (&'a String, &'a SessionRec)>,
    topic: &str,
    repos: &[&str],
) -> Option<Found> {
    sessions
        .into_iter()
        .filter(|(_, rec)| !rec.identifier.is_empty() && rec.identifier.eq_ignore_ascii_case(topic))
        .filter(|(_, rec)| in_repos(rec, repos))
        .map(|(sid, rec)| (!rec.closed, rec.prompted_at, sid))
        .max()
        .map(|(open, _, sid)| Found::new(open, sid))
}

/// Whether `rec` works in one of `repos` (names, which ignore case). A session with no
/// repository recorded is in none.
fn in_repos(rec: &SessionRec, repos: &[&str]) -> bool {
    rec.repo
        .as_deref()
        .is_some_and(|r| repos.iter().any(|n| n.eq_ignore_ascii_case(r)))
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

    fn pr(number: u64) -> PullRequestRef {
        PullRequestRef {
            repo: "o/r".into(),
            number,
            url: format!("https://github.com/o/r/pull/{number}"),
        }
    }

    fn rec(branch: &str, prompted_at: u64) -> SessionRec {
        SessionRec {
            repo: Some("app".into()),
            branch: Some(branch.into()),
            prompted_at,
            ..SessionRec::default()
        }
    }

    fn opened_for(number: u64, branch: &str, prompted_at: u64) -> SessionRec {
        SessionRec {
            pull_request: Some(pr(number)),
            ..rec(branch, prompted_at)
        }
    }

    #[test]
    fn closed_sessions_get_no_feedback() {
        let closed = |r: SessionRec| SessionRec { closed: true, ..r };
        let done = HashMap::from([("done".to_string(), closed(rec("en-593", 9)))]);
        assert_eq!(
            find(&done, &pr(7), "en-593", &["app"]),
            Some(Found::Closed("done".into()))
        );
        let reopened = HashMap::from([
            ("done".to_string(), closed(rec("en-593", 9))),
            ("again".to_string(), rec("en-593", 1)),
        ]);
        assert_eq!(
            find(&reopened, &pr(7), "en-593", &["app"]),
            Some(Found::Open("again".into())),
            "an open session wins over a newer closed one"
        );
        let opened = HashMap::from([
            ("pr".to_string(), closed(opened_for(7, "en-593", 1))),
            ("issue".to_string(), rec("en-593", 9)),
        ]);
        assert_eq!(
            find(&opened, &pr(7), "en-593", &["app"]),
            Some(Found::Closed("pr".into())),
            "the pull request's own session wins even closed"
        );
    }

    #[test]
    fn each_pull_request_has_its_own_session() {
        let sessions = HashMap::from([
            ("old".to_string(), rec("en-593", 1)),
            ("new".to_string(), rec("en-593", 2)),
            ("pr8".to_string(), opened_for(8, "en-593-3", 0)),
            ("pr9".to_string(), opened_for(9, "en-593", 0)),
            ("chat".to_string(), SessionRec::default()),
        ]);
        let find = |number, head| open(find(&sessions, &pr(number), head, &["app"]));
        assert_eq!(
            find(7, "en-593").as_deref(),
            Some("new"),
            "newest on the branch"
        );
        assert_eq!(find(9, "en-593").as_deref(), Some("pr9"), "opened for it");
        assert_eq!(find(8, "en-593-3").as_deref(), Some("pr8"));
        assert_eq!(find(10, "en-593-3"), None, "pr8's branch is not its own");
        assert_eq!(find(11, "en-593-4"), None, "stacked branches are not owned");
        assert_eq!(find(12, "main"), None);
        let other_repo = PullRequestRef {
            repo: "o/lib".into(),
            ..pr(9)
        };
        assert_eq!(
            open(super::find(&sessions, &other_repo, "x", &["app"])),
            None,
            "same number in another repository"
        );
        let any_case = PullRequestRef {
            repo: "O/R".into(),
            ..pr(9)
        };
        assert_eq!(
            open(super::find(&sessions, &any_case, "x", &["app"])).as_deref(),
            Some("pr9")
        );
    }

    #[test]
    fn the_newest_open_session_on_the_topic_in_the_project() {
        let rec = |identifier: &str, repo: &str, prompted_at, closed| SessionRec {
            identifier: identifier.into(),
            repo: Some(repo.into()),
            prompted_at,
            closed,
            ..SessionRec::default()
        };
        let sessions = HashMap::from([
            ("old".to_string(), rec("SH-8", "app", 1, false)),
            ("new".to_string(), rec("SH-8", "app", 2, false)),
            ("done".to_string(), rec("SH-8", "app", 9, true)),
            ("lib".to_string(), rec("SH-8", "lib", 5, false)),
            ("other".to_string(), rec("SH-80", "app", 9, false)),
            ("chat".to_string(), SessionRec::default()),
        ]);
        let found = |topic, repos: &[&str]| for_topic(&sessions, topic, repos);
        assert_eq!(found("sh-8", &["app"]), Some(Found::Open("new".into())));
        assert_eq!(found("SH-8", &["lib"]), Some(Found::Open("lib".into())));
        assert_eq!(found("SH-8", &["web"]), None, "another repository");
        assert_eq!(found("SH", &["app"]), None);
        assert_eq!(found("", &["app"]), None);
        let closed = HashMap::from([("done".to_string(), rec("SH-8", "app", 9, true))]);
        assert_eq!(
            for_topic(&closed, "SH-8", &["app"]),
            Some(Found::Closed("done".into()))
        );
    }
}
