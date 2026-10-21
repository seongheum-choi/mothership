//! Starting a session's turn: record the session, settle its repository and mode, then run
//! the prompt, or hold it with the replies that follow until the session can start.

use super::routing::{Blocked, IssueLookup};
use crate::{app::App, session::choice, store::SessionRec};
use serde_json::{Value, json};
use std::sync::Arc;

/// Records the session, settles its repository and mode, then runs the prompt, or keeps it, and
/// every reply after it, until a reply settles whatever is missing.
pub(super) async fn start_turn(
    app: &Arc<App>,
    sid: &str,
    issue: &Value,
    prompt: String,
    request: Option<&str>,
) {
    let field = |k: &str| issue[k].as_str().unwrap_or_default().to_string();
    let opening = app.store.update(|s| {
        let rec = s.sessions.entry(sid.to_string()).or_default();
        let opening = rec.issue_id.is_empty();
        if opening {
            *rec = SessionRec {
                issue_id: field("id"),
                identifier: field("identifier"),
                title: field("title"),
                url: field("url"),
                mode_pending: true,
                ..std::mem::take(rec)
            };
        }
        rec.prompted_at = crate::store::now_secs();
        rec.closed = false;
        opening
    });
    let linear = &app.linear.surface;
    let mut lookup = IssueLookup::new(issue["id"].as_str().unwrap_or_default());
    let directives = choice::record(app, sid, request.unwrap_or_default());
    // A refused directive comes back corrected in a new reply, so keeping the refused one too
    // would give the agent the request twice. The opening prompt is kept all the same: Linear's
    // issue context comes only with it.
    let keep = opening || directives.is_ok();
    let ready = match directives.map_err(Blocked::Stuck) {
        Ok(()) => match linear
            .choose_repo(app, sid, issue, request, &mut lookup)
            .await
        {
            Ok(()) => linear.choose_mode(app, sid, &mut lookup).await,
            blocked => blocked,
        },
        blocked => blocked,
    };
    if let Err(blocked) = ready {
        if keep {
            app.store.update(|s| {
                if let Some(rec) = s.sessions.get_mut(sid) {
                    hold(&mut rec.pending_prompt, prompt);
                }
            });
        }
        let content = match blocked {
            Blocked::Ask(question) => {
                tracing::info!("[{sid}] waiting: {question}");
                json!({ "type": "elicitation", "body": question })
            }
            Blocked::Stuck(problem) => {
                tracing::warn!("[{sid}] cannot start: {problem}");
                json!({ "type": "error", "body": problem })
            }
        };
        linear.activity(app, sid, content, false).await;
        return;
    }
    let pending = app.store.update(|s| {
        s.sessions
            .get_mut(sid)
            .and_then(|rec| rec.pending_prompt.take())
    });
    let prompt = match pending {
        Some(first) => format!("{first}\n\n{prompt}"),
        None => prompt,
    };
    app.linear.submit(app, sid, prompt, ());
}

/// Runs every prompt held while the instance drained for a restart.
pub fn release(app: &Arc<App>) {
    let held: Vec<(String, String)> = app.store.update(|s| {
        s.sessions
            .iter_mut()
            .filter_map(|(sid, rec)| Some((sid.clone(), rec.held.take()?)))
            .collect()
    });
    for (sid, prompt) in held {
        tracing::info!("[{sid}] running the prompt held for the restart");
        app.linear.submit(app, &sid, prompt, ());
    }
}

/// Adds `prompt` to what a session that cannot start yet keeps for its first turn: Linear's
/// issue context, then every reply in order, including those that did not settle anything,
/// except replies whose directives were refused.
pub(super) fn hold(pending: &mut Option<String>, prompt: String) {
    *pending = Some(match pending.take() {
        Some(held) => format!("{held}\n\n{prompt}"),
        None => prompt,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_replies_are_kept_in_order() {
        let mut pending = None;
        hold(&mut pending, "context of SH-4".into());
        hold(&mut pending, "it is the login page".into());
        hold(&mut pending, "[repo=nope]".into());
        assert_eq!(
            pending.as_deref(),
            Some("context of SH-4\n\nit is the login page\n\n[repo=nope]")
        );
    }
}
