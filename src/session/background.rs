//! Background tasks an agent still runs when it answers, and how long it may keep running for
//! them.
//!
//! Claude Code wakes the agent when a background task (an `Agent` call, a background `Bash`)
//! finishes and answers again. With input closed, Claude Code 2.1.304 stops a background agent
//! still running ten minutes in and exits, so input stays open past an answer while any task
//! runs, up to a limit.

use std::time::Duration;
use tokio::time::Instant;

pub(super) struct Background {
    live: usize,
    timeout: Duration,
    /// When the agent's input closes anyway, set while it waits for its tasks.
    deadline: Option<Instant>,
}

/// What follows an answer.
#[derive(Debug, PartialEq)]
pub(super) enum Next {
    /// Nothing runs in the background: input may close.
    Close,
    /// Tasks still run and a wait for them starts; the note says so.
    Wait(String),
    /// Tasks still run and the wait that started at an earlier answer goes on.
    KeepWaiting,
}

impl Background {
    pub(super) fn new(timeout: Duration) -> Self {
        Self {
            live: 0,
            timeout,
            deadline: None,
        }
    }

    /// How many tasks run now, as the agent last reported.
    pub(super) fn set_live(&mut self, live: usize) {
        self.live = live;
    }

    pub(super) fn answered(&mut self) -> Next {
        if self.live == 0 {
            self.deadline = None;
            return Next::Close;
        }
        if self.deadline.is_some() {
            return Next::KeepWaiting;
        }
        self.deadline = Some(Instant::now() + self.timeout);
        Next::Wait(format!(
            "Waiting for {} background task(s) to finish before the agent exits.",
            self.live
        ))
    }

    pub(super) fn waiting(&self) -> bool {
        self.deadline.is_some()
    }

    /// When the wait ends; only meaningful while [`waiting`](Self::waiting).
    pub(super) fn deadline(&self) -> Instant {
        self.deadline.unwrap_or_else(Instant::now)
    }

    /// Ends the wait at its deadline, with the note for the thread.
    pub(super) fn give_up(&mut self) -> String {
        self.deadline = None;
        format!(
            "Background work is still running after {}s; closing the agent's input so it can exit.",
            self.timeout.as_secs()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_once_for_tasks_still_running() {
        let mut background = Background::new(Duration::from_secs(60));
        assert_eq!(background.answered(), Next::Close);
        assert!(!background.waiting());

        background.set_live(2);
        assert_eq!(
            background.answered(),
            Next::Wait("Waiting for 2 background task(s) to finish before the agent exits.".into())
        );
        let deadline = background.deadline();
        // A later answer while tasks still run keeps the first deadline.
        background.set_live(1);
        assert_eq!(background.answered(), Next::KeepWaiting);
        assert_eq!(background.deadline(), deadline);

        background.set_live(0);
        assert_eq!(background.answered(), Next::Close);
        assert!(!background.waiting());
    }

    #[test]
    fn gives_up_at_the_deadline() {
        let mut background = Background::new(Duration::from_secs(5));
        background.set_live(1);
        background.answered();
        assert!(background.waiting());
        assert!(
            background
                .give_up()
                .starts_with("Background work is still running after 5s")
        );
        assert!(!background.waiting());
    }
}
