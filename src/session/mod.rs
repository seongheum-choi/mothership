//! Conversation lifecycle shared by every surface (Linear, Zulip, ...): one worker per
//! conversation runs agent turns back to back, feeds prompts that arrive mid-turn into the
//! running agent, and reports progress and results through the surface.

mod relay;
mod turn;

use crate::{agent::Launch, app::App};
use anyhow::Result;
use serde_json::Value;
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use turn::worker;

/// Where requests come from and where progress and results go.
pub trait Surface: Send + Sync + Sized + 'static {
    /// Per-prompt data handed back when the turn that answered the prompt ends
    /// (for example a chat message id to mark as handled).
    type Ticket: Send + 'static;

    /// Prepares the workspace and launch settings for the next turn of `key`.
    fn launch(&self, app: &Arc<App>, key: &str) -> impl Future<Output = Result<Launch>> + Send;

    /// Progress while a turn runs. Surfaces ignore what they cannot show.
    fn update(&self, app: &Arc<App>, key: &str, update: Update) -> impl Future<Output = ()> + Send;

    /// How a turn ended, with the tickets of the prompts it answered.
    fn finish(
        &self,
        app: &Arc<App>,
        key: &str,
        tickets: Vec<Self::Ticket>,
        outcome: Outcome,
    ) -> impl Future<Output = ()> + Send;
}

#[derive(Debug, PartialEq)]
pub enum Update {
    /// A turn is starting.
    Working,
    /// The agent's interim text. `nested` marks a subagent.
    Thought { text: String, nested: bool },
    Tool {
        name: String,
        input: Value,
        nested: bool,
    },
    /// A line a tool or skill wrote to `MOTHERSHIP_PROGRESS_FILE`.
    Progress(String),
    /// A prompt that arrived mid-turn was passed to the running agent.
    Noted,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Reply(String),
    Failed(String),
    /// Stopped by request; the note, when there is one, says why.
    Stopped(Option<String>),
}

pub enum Msg<T> {
    Prompt { text: String, ticket: T },
    Stop(Option<String>),
}

/// Running conversations of one surface.
pub struct Registry<S: Surface> {
    pub surface: S,
    live: Mutex<HashMap<String, mpsc::UnboundedSender<Msg<S::Ticket>>>>,
}

impl<S: Surface> Registry<S> {
    pub fn new(surface: S) -> Self {
        Self {
            surface,
            live: Mutex::default(),
        }
    }

    /// Hands `text` to the conversation's worker, starting one if it is idle.
    pub fn submit(self: &Arc<Self>, app: &Arc<App>, key: &str, text: String, ticket: S::Ticket) {
        // Senders only send while holding `live`, and a worker only exits after finding its
        // queue empty under the same lock, so no prompt lands in a channel nobody reads.
        let mut live = self.live.lock().expect("live lock poisoned");
        let msg = Msg::Prompt { text, ticket };
        let msg = match live.get(key) {
            Some(tx) => match tx.send(msg) {
                Ok(()) => return,
                Err(mpsc::error::SendError(msg)) => msg,
            },
            None => msg,
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(msg);
        live.insert(key.to_string(), tx);
        tokio::spawn(worker(app.clone(), self.clone(), key.to_string(), rx));
    }

    /// Stops the running turn, reporting `Outcome::Stopped(note)`. Returns false when nothing
    /// was running.
    pub fn stop(&self, key: &str, note: Option<String>) -> bool {
        let live = self.live.lock().expect("live lock poisoned");
        live.get(key)
            .is_some_and(|tx| tx.send(Msg::Stop(note)).is_ok())
    }

    /// Stops every running conversation. Returns how many were told to stop; each reports
    /// `Outcome::Stopped` and its worker leaves `live` once its agent has been killed.
    pub fn stop_all(&self) -> usize {
        let live = self.live.lock().expect("live lock poisoned");
        live.values()
            .filter(|tx| tx.send(Msg::Stop(None)).is_ok())
            .count()
    }

    pub fn busy(&self) -> bool {
        !self.live.lock().expect("live lock poisoned").is_empty()
    }
}
