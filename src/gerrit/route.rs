//! Which session a Gerrit review continues, and starting its turn.

use super::{api, event::Event, event::mentions, prompt::prompt};
use crate::{
    app::App,
    github::{Allowance, PROMPTS_PER_WINDOW},
    sessions::{self, Found},
    store::SessionRec,
};
use serde_json::json;
use std::{sync::Arc, time::Instant};

pub(super) async fn handle(app: Arc<App>, event: Event) {
    let cfg = app
        .cfg
        .gerrit
        .as_ref()
        .expect("gerrit handler runs only when enabled");
    let gerrit = app
        .gerrit
        .as_ref()
        .expect("gerrit handler runs only when enabled");
    let change = &event.url;
    let topic = &event.topic;
    let projects = gerrit.projects();
    let names = projects.names(&event.project);
    let found = app.store.read(|s| find(&s.sessions, topic, &names));
    let sid = match found {
        Some(Found::Open(sid)) => sid,
        Some(Found::Closed(sid)) => {
            tracing::info!("[{sid}] gerrit {change} on {topic}: the issue is closed, ignored");
            return;
        }
        None => {
            tracing::info!("gerrit {change} on {topic}: no session for this topic, ignored");
            return;
        }
    };
    let comments = match api::review(&app, cfg, &event).await {
        Ok(Some(comments)) => comments,
        Ok(None) => {
            tracing::debug!("[{sid}] gerrit {change}: the agent's own reply, ignored");
            return;
        }
        Err(e) => {
            tracing::warn!("[{sid}] gerrit {change}: fetching the review failed: {e:#}");
            return;
        }
    };
    let addressed = mentions(&event.message, &cfg.mention)
        || comments.iter().any(|c| mentions(&c.message, &cfg.mention));
    if !addressed {
        // Debug: most review traffic is not for the agent.
        tracing::debug!(
            "[{sid}] gerrit review by {} on {change}: not addressed to the agent, ignored",
            event.author.label()
        );
        return;
    }
    let allowed = gerrit
        .budget
        .lock()
        .expect("prompt budget poisoned")
        .take(&sid, Instant::now());
    match allowed {
        Allowance::Granted => {}
        Allowance::Denied { notify: false } => {
            tracing::info!("[{sid}] gerrit feedback paused, {change} ignored");
            return;
        }
        Allowance::Denied { notify: true } => {
            tracing::warn!(
                "[{sid}] {PROMPTS_PER_WINDOW} Gerrit prompts within the hour; pausing Gerrit feedback"
            );
            app.linear.stop(&sid, None);
            let body = format!(
                "Paused Gerrit feedback: this session got {PROMPTS_PER_WINDOW} prompts from \
                 Gerrit within an hour, which looks like the agent answering its own comments. \
                 The running turn was stopped, and Gerrit reviews on topic {topic} are ignored \
                 until the hour is up. Prompt here to continue."
            );
            app.linear
                .surface
                .activity(&app, &sid, json!({ "type": "error", "body": body }), false)
                .await;
            return;
        }
    }
    tracing::info!(
        "[{sid}] gerrit review from {} on {change} ({} comments)",
        event.author.label(),
        comments.len()
    );
    sessions::mark_prompted(&app.store, &sid);
    app.linear.submit(&app, &sid, prompt(&event, &comments), ());
}

/// The newest session on the issue whose key is `topic`, in one of `repos` (names): every
/// layer of a stack is pushed with the issue key as topic, so the issue's newest session hears
/// all of them. An open session wins over a closed one.
fn find<'a>(
    sessions: impl IntoIterator<Item = (&'a String, &'a SessionRec)>,
    topic: &str,
    repos: &[&str],
) -> Option<Found> {
    sessions
        .into_iter()
        .filter(|(_, rec)| !rec.identifier.is_empty() && rec.identifier.eq_ignore_ascii_case(topic))
        .filter(|(_, rec)| {
            rec.repo
                .as_deref()
                .is_some_and(|r| repos.iter().any(|n| n.eq_ignore_ascii_case(r)))
        })
        .map(|(sid, rec)| (!rec.closed, rec.prompted_at, sid))
        .max()
        .map(|(open, _, sid)| {
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
        let found = |topic, repos: &[&str]| find(&sessions, topic, repos);
        assert_eq!(found("sh-8", &["app"]), Some(Found::Open("new".into())));
        assert_eq!(found("SH-8", &["lib"]), Some(Found::Open("lib".into())));
        assert_eq!(found("SH-8", &["web"]), None, "another repository");
        assert_eq!(found("SH", &["app"]), None);
        assert_eq!(found("", &["app"]), None);
        let closed = HashMap::from([("done".to_string(), rec("SH-8", "app", 9, true))]);
        assert_eq!(
            find(&closed, "SH-8", &["app"]),
            Some(Found::Closed("done".into()))
        );
    }
}
