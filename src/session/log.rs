//! What the service log says about each turn: what it runs and how it ended, so a session's
//! history reads from the log without the surface it reported to.

use super::{Outcome, Surface, turn::Turn};
use crate::agent::Launch;
use std::time::Duration;

impl<S: Surface> Turn<'_, S> {
    /// Logs what the turn runs: on which runner and model, where, and whether it resumes.
    pub(super) fn log_start(&self, launch: &Launch) {
        let label = self.app.store.read(|s| {
            let rec = s.sessions.get(self.key)?;
            let label = if rec.identifier.is_empty() {
                &rec.title
            } else {
                &rec.identifier
            };
            Some(label.clone()).filter(|l| !l.is_empty())
        });
        let models = &launch.models;
        tracing::info!(
            "[{}] turn{}: {} {}{} in {}, {}",
            self.key,
            label.map(|l| format!(" on {l}")).unwrap_or_default(),
            launch.runner,
            models.model,
            models
                .effort
                .as_ref()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default(),
            launch.cwd.display(),
            launch
                .resume
                .as_ref()
                .map_or_else(|| "new session".to_string(), |id| format!("resuming {id}")),
        );
    }

    /// Reports how the turn ended to the surface, and to the log with how long it ran.
    pub(super) async fn finish(&self, tickets: Vec<S::Ticket>, outcome: Outcome) {
        let (key, took) = (self.key, took(self.started.elapsed()));
        match &outcome {
            Outcome::Reply(_) => tracing::info!("[{key}] turn answered after {took}"),
            Outcome::Failed(why) => tracing::error!("[{key}] turn failed after {took}: {why}"),
            Outcome::Stopped(note) => tracing::info!(
                "[{key}] turn stopped after {took}{}",
                note.as_ref().map(|n| format!(": {n}")).unwrap_or_default()
            ),
        }
        self.reg
            .surface
            .finish(self.app, key, tickets, outcome)
            .await;
    }
}

/// A turn's length the way people say it: `45s`, `3m12s`, `1h05m`.
fn took(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match (secs / 3600, secs / 60 % 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, _) => format!("{h}h{m:02}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_how_long_a_turn_took() {
        let took = |secs| took(Duration::from_secs(secs));
        assert_eq!(took(0), "0s");
        assert_eq!(took(59), "59s");
        assert_eq!(took(60), "1m00s");
        assert_eq!(took(192), "3m12s");
        assert_eq!(took(3600), "1h00m");
        assert_eq!(took(3_959), "1h05m");
    }
}
