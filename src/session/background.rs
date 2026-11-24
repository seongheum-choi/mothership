//! Background tasks an agent still runs when it answers, how long it may keep running for
//! them, and what the surface hears of the wait.
//!
//! Claude Code wakes the agent when a background task (an `Agent` call, a background `Bash`)
//! finishes and answers again. With input closed, Claude Code 2.1.304 stops a background agent
//! still running ten minutes in and exits, so input stays open past an answer while any task
//! runs, up to a limit. The surface is told when such a wait starts and how it ends.

use super::{Surface, Update, turn::Turn};
use crate::agent::Agent;
use std::time::Duration;
use tokio::time::Instant;

pub(super) struct Background {
    live: usize,
    timeout: Duration,
    /// When the agent's input closes anyway, set while it waits for its tasks.
    deadline: Option<Instant>,
}

/// Where an agent's wait for its background tasks stands, as the surface is told.
#[derive(Debug, PartialEq)]
pub enum Wait {
    /// It answered while tasks still run, and its input stays open for them.
    Started { tasks: usize },
    /// Its tasks finished and it answered again.
    Finished,
    /// The wait ended before the tasks did; the note says why.
    Abandoned(String),
}

impl Wait {
    /// The wait in words, for the thread.
    pub fn note(&self) -> String {
        match self {
            Self::Started { tasks } => {
                format!("Waiting for {tasks} background task(s) to finish before the agent exits.")
            }
            Self::Finished => "Background tasks finished.".into(),
            Self::Abandoned(why) => why.clone(),
        }
    }
}

/// What follows an answer.
#[derive(Debug, PartialEq)]
enum Next {
    /// Nothing runs in the background: input may close.
    Close,
    /// Tasks still run and a wait for them starts.
    Wait(Wait),
    /// Tasks still run and the wait that started at an earlier answer goes on.
    KeepWaiting,
    /// The tasks a wait was for have finished: input may close, and the wait is over.
    Finished,
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

    fn answered(&mut self) -> Next {
        match (self.live, self.deadline.is_some()) {
            (0, false) => Next::Close,
            (0, true) => {
                self.deadline = None;
                Next::Finished
            }
            (_, true) => Next::KeepWaiting,
            (tasks, false) => {
                self.deadline = Some(Instant::now() + self.timeout);
                Next::Wait(Wait::Started { tasks })
            }
        }
    }

    pub(super) fn waiting(&self) -> bool {
        self.deadline.is_some()
    }

    /// When the wait ends; only meaningful while [`waiting`](Self::waiting).
    pub(super) fn deadline(&self) -> Instant {
        self.deadline.unwrap_or_else(Instant::now)
    }

    /// Ends the wait at its deadline.
    pub(super) fn give_up(&mut self) -> Wait {
        self.deadline = None;
        Wait::Abandoned(format!(
            "Background work is still running after {}s; closing the agent's input so it can exit.",
            self.timeout.as_secs()
        ))
    }

    /// Ends a wait the agent leaves by exiting or being stopped, so it does not read as going
    /// on; `None` when it was not waiting.
    fn left(&mut self) -> Option<Wait> {
        self.deadline.take().map(|_| {
            Wait::Abandoned("The agent ended before its background tasks finished.".into())
        })
    }
}

impl<S: Surface> Turn<'_, S> {
    /// After an answer: closes the agent's input, or keeps it open for the background tasks it
    /// still runs. The surface hears when the wait starts and when it ends.
    pub(super) async fn answered(&self, agent: &mut Agent, background: &mut Background) {
        let wait = match background.answered() {
            Next::Close => {
                agent.close_input();
                return;
            }
            Next::KeepWaiting => return,
            Next::Finished => {
                agent.close_input();
                Wait::Finished
            }
            Next::Wait(wait) => wait,
        };
        self.tell_wait(wait).await;
    }

    /// Tells the surface a wait the agent leaves by ending is over.
    pub(super) async fn left(&self, background: &mut Background) {
        if let Some(wait) = background.left() {
            self.tell_wait(wait).await;
        }
    }

    /// Logs where a wait for background tasks stands and tells the surface.
    pub(super) async fn tell_wait(&self, wait: Wait) {
        let note = wait.note();
        if let Wait::Abandoned(_) = wait {
            tracing::warn!("[{}] {note}", self.key);
        } else {
            tracing::info!("[{}] {note}", self.key);
        }
        let waiting = matches!(wait, Wait::Started { .. });
        self.reg.note(self.key, |a| a.waiting = waiting);
        let update = Update::Waiting(wait);
        self.reg.surface.update(self.app, self.key, update).await;
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
        let started = Wait::Started { tasks: 2 };
        assert_eq!(
            started.note(),
            "Waiting for 2 background task(s) to finish before the agent exits."
        );
        assert_eq!(background.answered(), Next::Wait(started));
        let deadline = background.deadline();
        // A later answer while tasks still run keeps the first deadline.
        background.set_live(1);
        assert_eq!(background.answered(), Next::KeepWaiting);
        assert_eq!(background.deadline(), deadline);

        // The answer after the tasks finished ends the wait, once.
        background.set_live(0);
        assert_eq!(background.answered(), Next::Finished);
        assert!(!background.waiting());
        assert_eq!(background.answered(), Next::Close);
        assert_eq!(background.left(), None);
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
                .note()
                .starts_with("Background work is still running after 5s")
        );
        assert!(!background.waiting());
    }

    #[test]
    fn an_agent_ending_mid_wait_ends_the_wait() {
        let mut background = Background::new(Duration::from_secs(5));
        background.set_live(1);
        background.answered();
        assert!(matches!(background.left(), Some(Wait::Abandoned(_))));
        assert!(!background.waiting());
        assert_eq!(background.left(), None);
    }
}
