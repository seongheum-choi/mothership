//! What the service log says about each turn: what it runs and how it ended, so a session's
//! history reads from the log without the surface it reported to.

use super::{Outcome, Surface, turn::Turn};
use crate::agent::Launch;
use std::time::Duration;

impl<S: Surface> Turn<'_, S> {
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
            label.map_or_default(|l| format!(" on {l}")),
            launch.runner,
            models.model,
            models.effort.as_ref().map_or_default(|e| format!(" ({e})")),
            launch.cwd.display(),
            launch
                .resume
                .as_ref()
                .map_or_else(|| "new session".to_string(), |id| format!("resuming {id}")),
        );
    }

    /// Reports how the turn ended to the surface, and to the log with how long it ran.
    pub(super) async fn finish(&self, tickets: Vec<S::Ticket>, outcome: Outcome) {
        self.report(tickets, outcome, "").await;
    }

    /// Reports one answer of the agent, unless it repeats the last one and answers no new
    /// prompt: Claude Code woken by its background work may give the same result again.
    pub(super) async fn answer(
        &self,
        answers: &mut Answers,
        tickets: Vec<S::Ticket>,
        outcome: Outcome,
    ) {
        let answer = answers.record(&outcome, tickets.len());
        if answer.repeats {
            let took = took(self.started.elapsed());
            tracing::info!(
                "[{}] turn answered after {took}{}",
                self.key,
                answer.context()
            );
            return;
        }
        self.report(tickets, outcome, &answer.context()).await;
    }

    async fn report(&self, tickets: Vec<S::Ticket>, outcome: Outcome, context: &str) {
        let (key, took) = (self.key, took(self.started.elapsed()));
        match &outcome {
            Outcome::Reply(_) => tracing::info!("[{key}] turn answered after {took}{context}"),
            Outcome::Failed(why) => {
                tracing::error!("[{key}] turn failed after {took}{context}: {why}");
            }
            Outcome::Stopped(note) => tracing::info!(
                "[{key}] turn stopped after {took}{context}{}",
                note.as_ref().map_or_default(|n| format!(": {n}"))
            ),
        }
        self.reg
            .surface
            .finish(self.app, key, tickets, outcome)
            .await;
    }
}

/// The answers of one agent process so far.
#[derive(Default)]
pub(super) struct Answers {
    count: usize,
    /// The last one reported.
    last: Option<Outcome>,
}

/// One answer, as [`Answers::record`] judged it.
#[derive(Debug, PartialEq)]
pub(super) struct Answer {
    /// Which answer of the agent this is, from 1.
    number: usize,
    /// Prompts it answers that no earlier answer did.
    prompts: usize,
    /// It is the last reported answer again, to no new prompt, so it is not reported.
    repeats: bool,
}

impl Answers {
    pub(super) fn record(&mut self, outcome: &Outcome, prompts: usize) -> Answer {
        self.count += 1;
        let repeats = prompts == 0 && self.last.as_ref() == Some(outcome);
        if !repeats {
            self.last = Some(outcome.clone());
        }
        Answer {
            number: self.count,
            prompts,
            repeats,
        }
    }
}

impl Answer {
    /// What the log line about the answer adds: nothing for the agent's first, else which
    /// answer it is and why it came.
    fn context(&self) -> String {
        match (self.number, self.prompts, self.repeats) {
            (1, _, _) => String::new(),
            (n, 0, true) => {
                format!(
                    " (answer {n} of the agent, unasked and the same as the last: not reported again)"
                )
            }
            (n, 0, false) => {
                format!(" (answer {n} of the agent, unasked: woken by its background work)")
            }
            (n, p, _) => format!(" (answer {n} of the agent, to {p} prompt(s) that joined it)"),
        }
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

    /// A real agent's answers: its first, one woken by a background task, one to a prompt that
    /// joined it, and that answer twice more within a second.
    #[test]
    fn an_answer_repeated_unasked_is_not_reported_again() {
        let reply = |text: &str| Outcome::Reply(text.into());
        let mut answers = Answers::default();
        let seen: Vec<_> = [
            (reply("Started the build."), 1),
            (reply("The build passed."), 0),
            (reply("Done, and the PR is open."), 1),
            (reply("Done, and the PR is open."), 0),
            (reply("Done, and the PR is open."), 0),
        ]
        .iter()
        .map(|(outcome, prompts)| answers.record(outcome, *prompts))
        .collect();
        let reported: Vec<_> = seen.iter().map(|a| !a.repeats).collect();
        assert_eq!(reported, [true, true, true, false, false]);
        assert_eq!(seen[0].context(), "");
        assert_eq!(
            seen[1].context(),
            " (answer 2 of the agent, unasked: woken by its background work)"
        );
        assert_eq!(
            seen[2].context(),
            " (answer 3 of the agent, to 1 prompt(s) that joined it)"
        );
        assert_eq!(
            seen[4].context(),
            " (answer 5 of the agent, unasked and the same as the last: not reported again)"
        );
        // A different answer is reported, and the same text answering a new prompt is too.
        assert!(!answers.record(&reply("Something else."), 0).repeats);
        assert!(!answers.record(&reply("Something else."), 1).repeats);
        assert!(answers.record(&reply("Something else."), 0).repeats);
    }
}
