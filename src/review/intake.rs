//! Review feedback that continues a Linear session, from GitHub or Gerrit: a capped number of
//! prompts per session per hour, and starting the turn.

use crate::{app::App, sessions};
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Review-started prompts a session may take per window before it is paused, so a reply loop
/// the marker misses (the agent posting with plain `gh`) burns out quickly.
const PROMPTS_PER_WINDOW: usize = 10;
const WINDOW: Duration = Duration::from_hours(1);

/// Review-started prompts per session within the last [`WINDOW`].
#[derive(Default)]
pub struct Budget {
    granted: HashMap<String, VecDeque<Instant>>,
    /// When each session was last told it is paused.
    notified: HashMap<String, Instant>,
}

#[derive(Debug, PartialEq)]
enum Allowance {
    Granted,
    /// `notify` is true for the first refusal of a pause, so Linear hears of it once.
    Denied {
        notify: bool,
    },
}

impl Budget {
    fn take(&mut self, key: &str, now: Instant) -> Allowance {
        let granted = self.granted.entry(key.to_string()).or_default();
        while granted
            .front()
            .is_some_and(|&t| now.duration_since(t) >= WINDOW)
        {
            granted.pop_front();
        }
        if granted.len() < PROMPTS_PER_WINDOW {
            granted.push_back(now);
            return Allowance::Granted;
        }
        let notify = self
            .notified
            .get(key)
            .is_none_or(|&t| now.duration_since(t) >= WINDOW);
        if notify {
            self.notified.insert(key.to_string(), now);
        }
        Allowance::Denied { notify }
    }
}

/// A review event's prompt for the session that owns its change.
pub struct Prompt<'a> {
    /// `GitHub` or `Gerrit`, as the logs and the pause notice name the source.
    pub source: &'a str,
    /// Who or what is ignored while the session is paused, for the log: `@alice`, a change URL.
    pub from: &'a str,
    /// What stays ignored until the hour is up, for the notice: `GitHub comments on this PR`.
    pub ignored: &'a str,
    pub text: String,
}

/// Starts a turn on `sid` with `prompt` unless `budget` has paused the session. The first
/// refusal of a pause stops the running turn and tells the session why. Returns whether the
/// prompt was submitted.
pub async fn submit(app: &Arc<App>, sid: &str, budget: &Mutex<Budget>, prompt: Prompt<'_>) -> bool {
    let Prompt {
        source,
        from,
        ignored,
        text,
    } = prompt;
    let allowed = budget
        .lock()
        .expect("prompt budget poisoned")
        .take(sid, Instant::now());
    match allowed {
        Allowance::Granted => {
            sessions::mark_prompted(&app.store, sid);
            app.linear.submit(app, sid, text, ());
            true
        }
        Allowance::Denied { notify: false } => {
            let source = source.to_lowercase();
            tracing::info!("[{sid}] {source} feedback paused, {from} ignored");
            false
        }
        Allowance::Denied { notify: true } => {
            tracing::warn!(
                "[{sid}] {PROMPTS_PER_WINDOW} {source} prompts within the hour; pausing {source} \
                 feedback"
            );
            app.linear.stop(sid, None);
            let body = format!(
                "Paused {source} feedback: this session got {PROMPTS_PER_WINDOW} prompts from \
                 {source} within an hour, which looks like the agent answering its own comments. \
                 The running turn was stopped, and {ignored} are ignored until the hour is up. \
                 Prompt here to continue."
            );
            let content = json!({ "type": "error", "body": body });
            app.linear.surface.activity(app, sid, content, false).await;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_pauses_a_session_after_too_many_prompts() {
        let mut budget = Budget::default();
        let t0 = Instant::now();
        for i in 0..PROMPTS_PER_WINDOW {
            let t = t0 + Duration::from_secs(i as u64);
            assert_eq!(budget.take("s", t), Allowance::Granted);
        }
        let later = t0 + Duration::from_secs(60);
        assert_eq!(budget.take("s", later), Allowance::Denied { notify: true });
        assert_eq!(budget.take("s", later), Allowance::Denied { notify: false });
        assert_eq!(budget.take("other", later), Allowance::Granted);
        // The first prompt leaves the window, freeing one slot.
        assert_eq!(budget.take("s", t0 + WINDOW), Allowance::Granted);
        assert_eq!(
            budget.take("s", t0 + WINDOW),
            Allowance::Denied { notify: false }
        );
        assert_eq!(
            budget.take("s", later + WINDOW + Duration::from_secs(10)),
            Allowance::Granted
        );
    }
}
