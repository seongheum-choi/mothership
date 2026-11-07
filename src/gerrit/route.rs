//! Which session a Gerrit review continues, and starting its turn.

use super::{api, event::Event, event::mentions, prompt::prompt};
use crate::{
    app::App,
    review::intake,
    sessions::{self, Found},
};
use std::sync::Arc;

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
    let found = app
        .store
        .read(|s| sessions::for_topic(&s.sessions, topic, &names));
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
    let ignored = format!("Gerrit reviews on topic {topic}");
    let prompt = intake::Prompt {
        source: "Gerrit",
        from: change,
        ignored: &ignored,
        text: prompt(&event, &comments),
    };
    if intake::submit(&app, &sid, &gerrit.budget, prompt).await {
        tracing::info!(
            "[{sid}] gerrit review from {} on {change} ({} comments)",
            event.author.label(),
            comments.len()
        );
    }
}
