//! What `mothership session` asks of the Linear surface: each agent session of an issue with
//! its turn and last report, and a follow-up posted in the newest session's thread that runs as
//! the session's next turn.

use super::{Linear, start::start_turn, thread::root_comment};
use crate::{app::App, session::Activity, store::SessionRec};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{fmt::Write as _, sync::Arc};

impl Linear {
    /// Each agent session of `issue`, newest first: its Linear status, what mothership runs for
    /// it, and its last report.
    pub async fn session_status(&self, app: &Arc<App>, issue: &str) -> Result<String> {
        let found = self.issue_sessions(app, issue).await?;
        let mut out = format!(
            "{} {}: {} agent session(s), newest first\n",
            found.issue["identifier"].as_str().unwrap_or(issue),
            found.issue["title"],
            found.sessions.len()
        );
        let now = crate::store::now_secs();
        for session in &found.sessions {
            let sid = session["id"].as_str().unwrap_or_default();
            let rec = app.store.read(|s| s.sessions.get(sid).cloned());
            let running = app.linear.running(sid);
            let turn = turn_state(rec.as_ref(), running, app.linear.activity(sid), now);
            let _ = write!(
                out,
                "\nsession {sid}\n  linear: {}, created {}\n  mothership: {turn}\n",
                session["status"].as_str().unwrap_or("unknown"),
                session["createdAt"].as_str().unwrap_or("unknown"),
            );
            match self.last_report(app, sid).await? {
                Some(report) => {
                    let _ = writeln!(out, "  last {} at {}:", report.kind, report.at);
                    for line in report.body.lines() {
                        let _ = writeln!(out, "    {line}");
                    }
                }
                None => out.push_str("  no report yet\n"),
            }
        }
        Ok(out)
    }

    /// Posts `body` under the root comment of `issue`'s newest session of this instance and
    /// runs it as that session's next turn, as a reply in the thread would. With `dry_run`,
    /// only says where it would go.
    pub async fn session_reply(
        &self,
        app: &Arc<App>,
        issue: &str,
        body: &str,
        dry_run: bool,
    ) -> Result<String> {
        if body.trim().is_empty() {
            bail!("the reply is empty; nothing was sent");
        }
        let found = self.issue_sessions(app, issue).await?;
        let (session, rec) = found
            .sessions
            .iter()
            .find_map(|session| {
                let sid = session["id"].as_str()?;
                let rec = app.store.read(|s| s.sessions.get(sid).cloned())?;
                Some((session, rec))
            })
            .with_context(|| format!("{issue} has no agent session of this instance"))?;
        let sid = session["id"].as_str().unwrap_or_default();
        let root = root_comment(session)
            .with_context(|| format!("session {sid} has no root comment to reply under"))?;
        let target = format!("session {sid} on {}, under comment {root}", rec.identifier);
        if dry_run {
            return Ok(format!("would reply in {target}; nothing was sent\n"));
        }
        let issue_id = found.issue["id"].as_str().unwrap_or(&rec.issue_id);
        // Held until the id is recorded, so the webhook Linear may send for the comment
        // finds it and does not run the reply a second time.
        let mut posted = self.posted_replies.lock().await;
        let comment = self.reply_in_thread(app, issue_id, root, body).await?;
        posted.insert(comment.clone());
        drop(posted);
        tracing::info!("[{sid}] reply {comment} from the CLI");
        start_turn(app, sid, &issue_of(&rec), body.to_string(), Some(body)).await;
        Ok(format!("replied in {target} (comment {comment})\n"))
    }

    /// Whether a `prompted` webhook is for a reply this instance posted itself and already
    /// runs. Forgets the reply, since Linear sends one webhook per comment.
    pub(super) async fn posted_here(&self, p: &Value) -> bool {
        let activity = &p["agentActivity"];
        let Some(comment) = activity["sourceCommentId"]
            .as_str()
            .or_else(|| activity["sourceComment"]["id"].as_str())
        else {
            return false;
        };
        self.posted_replies.lock().await.remove(comment)
    }
}

/// The issue fields a session's turn starts from, as a webhook would carry them.
fn issue_of(rec: &SessionRec) -> Value {
    json!({
        "id": rec.issue_id,
        "identifier": rec.identifier,
        "title": rec.title,
        "url": rec.url,
    })
}

/// What mothership is doing for a session: its turn, a prompt it holds, or nothing.
fn turn_state(rec: Option<&SessionRec>, running: bool, turn: Option<Activity>, now: u64) -> String {
    let Some(rec) = rec else {
        return "not a session of this instance".into();
    };
    match (running, turn) {
        (true, Some(turn)) if turn.waiting => format!(
            "waiting on {} background task(s), {}s into the turn",
            turn.background,
            now.saturating_sub(turn.started)
        ),
        (true, Some(turn)) => format!(
            "running for {}s, quiet for {}s, {} background task(s)",
            now.saturating_sub(turn.started),
            now.saturating_sub(turn.last_event),
            turn.background
        ),
        (true, None) => "starting a turn".into(),
        (false, _) if rec.held.is_some() => "idle, a prompt held for the restart".into(),
        (false, _) if rec.mode_pending || rec.pending_prompt.is_some() => {
            "idle, input blocked until its repository or mode is settled".into()
        }
        (false, _) if rec.closed => "idle, the issue is closed".into(),
        (false, _) => "idle".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(waiting: bool) -> Activity {
        Activity {
            workspace: None,
            started: 100,
            last_event: 150,
            background: 2,
            waiting,
        }
    }

    #[test]
    fn says_what_mothership_does_for_the_session() {
        let rec = SessionRec::default();
        assert_eq!(
            turn_state(None, false, None, 0),
            "not a session of this instance"
        );
        assert_eq!(
            turn_state(Some(&rec), true, Some(turn(false)), 160),
            "running for 60s, quiet for 10s, 2 background task(s)"
        );
        assert_eq!(
            turn_state(Some(&rec), true, Some(turn(true)), 160),
            "waiting on 2 background task(s), 60s into the turn"
        );
        assert_eq!(turn_state(Some(&rec), true, None, 0), "starting a turn");
        assert_eq!(turn_state(Some(&rec), false, None, 0), "idle");
    }

    #[test]
    fn says_why_an_idle_session_does_not_run() {
        let held = SessionRec {
            held: Some("go".into()),
            ..SessionRec::default()
        };
        assert!(turn_state(Some(&held), false, None, 0).contains("held for the restart"));
        let blocked = SessionRec {
            mode_pending: true,
            ..SessionRec::default()
        };
        assert!(turn_state(Some(&blocked), false, None, 0).contains("input blocked"));
        let closed = SessionRec {
            closed: true,
            ..SessionRec::default()
        };
        assert_eq!(
            turn_state(Some(&closed), false, None, 0),
            "idle, the issue is closed"
        );
    }
}
