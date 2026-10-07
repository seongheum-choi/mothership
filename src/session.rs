//! Conversation lifecycle shared by every surface (Linear, Zulip, ...): one worker per
//! conversation runs agent turns back to back, feeds prompts that arrive mid-turn into the
//! running agent, and reports progress and results through the surface.

use crate::{
    agent::{Agent, Event, Launch},
    app::App,
};
use anyhow::{Result, bail};
use serde_json::Value;
use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;

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

async fn worker<S: Surface>(
    app: Arc<App>,
    reg: Arc<Registry<S>>,
    key: String,
    mut rx: mpsc::UnboundedReceiver<Msg<S::Ticket>>,
) {
    let mut queued: Vec<(String, S::Ticket)> = Vec::new();
    loop {
        if queued.is_empty() {
            let msg = {
                let mut live = reg.live.lock().expect("live lock poisoned");
                if let Ok(msg) = rx.try_recv() {
                    msg
                } else {
                    live.remove(&key);
                    return;
                }
            };
            match msg {
                Msg::Prompt { text, ticket } => queued.push((text, ticket)),
                Msg::Stop(note) => {
                    reg.surface
                        .finish(&app, &key, Vec::new(), Outcome::Stopped(note))
                        .await;
                    continue;
                }
            }
        }
        let (texts, mut tickets): (Vec<String>, Vec<S::Ticket>) =
            std::mem::take(&mut queued).into_iter().unzip();
        let turn = Turn {
            app: &app,
            reg: &reg,
            key: &key,
        };
        if let Err(e) = turn
            .run(&texts.join("\n\n"), &mut tickets, &mut rx, &mut queued)
            .await
        {
            tracing::error!("[{key}] {e:#}");
            reg.surface
                .finish(&app, &key, tickets, Outcome::Failed(format!("{e:#}")))
                .await;
        }
    }
}

struct Turn<'a, S: Surface> {
    app: &'a Arc<App>,
    reg: &'a Registry<S>,
    key: &'a str,
}

impl<S: Surface> Turn<'_, S> {
    /// Runs one agent process. `tickets` holds prompts it is answering; on error the caller
    /// fails whatever is left. Prompts that arrive after its input closed go to `queued`.
    async fn run(
        &self,
        prompt: &str,
        tickets: &mut Vec<S::Ticket>,
        rx: &mut mpsc::UnboundedReceiver<Msg<S::Ticket>>,
        queued: &mut Vec<(String, S::Ticket)>,
    ) -> Result<()> {
        let (app, surface, key) = (self.app, &self.reg.surface, self.key);
        surface.update(app, key, Update::Working).await;
        let mut launch = surface.launch(app, key).await?;
        let _workspace = match app.lock_workspace(&launch.cwd).await {
            guard if launch.cwd.exists() => guard,
            guard => {
                // A closed issue's cleanup removed the worktree while this turn waited for it.
                drop(guard);
                launch = surface.launch(app, key).await?;
                app.lock_workspace(&launch.cwd).await
            }
        };
        let mut progress = ProgressTail::create(
            app.cfg
                .home
                .join("progress")
                .join(crate::store::file_name(key)),
        )?;
        launch.env.push((
            "MOTHERSHIP_PROGRESS_FILE".into(),
            progress.path.display().to_string(),
        ));

        let mut agent = Agent::spawn(&app.cfg.claude, launch)?;
        agent.send(prompt).await?;
        let mut relay = Relay::default();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                events = agent.next() => {
                    let Some(events) = events? else { break };
                    for event in events {
                        match event {
                            Event::Started { session_id } => app.store.update(|s| {
                                s.sessions.entry(key.to_string()).or_default().claude_session_id = Some(session_id);
                            }),
                            Event::Result { text, is_error } => {
                                agent.close_input();
                                for update in relay.flush_except(&text) {
                                    surface.update(app, key, update).await;
                                }
                                let outcome = if is_error { Outcome::Failed(text) } else { Outcome::Reply(text) };
                                surface.finish(app, key, std::mem::take(tickets), outcome).await;
                            }
                            event => {
                                for update in relay.on(event) {
                                    surface.update(app, key, update).await;
                                }
                            }
                        }
                    }
                }
                Some(msg) = rx.recv() => match msg {
                    Msg::Prompt { text, ticket } if agent.accepts_input() => {
                        agent.send(&text).await?;
                        tickets.push(ticket);
                        surface.update(app, key, Update::Noted).await;
                    }
                    Msg::Prompt { text, ticket } => queued.push((text, ticket)),
                    Msg::Stop(note) => {
                        agent.kill().await;
                        tickets.extend(queued.drain(..).map(|(_, ticket)| ticket));
                        surface.finish(app, key, std::mem::take(tickets), Outcome::Stopped(note)).await;
                        return Ok(());
                    }
                },
                _ = tick.tick() => {
                    for line in progress.read_new() {
                        surface.update(app, key, Update::Progress(line)).await;
                    }
                }
            }
        }
        for line in progress.read_new() {
            surface.update(app, key, Update::Progress(line)).await;
        }
        let status = agent.wait().await?;
        if !tickets.is_empty() {
            bail!("claude exited ({status}) without answering");
        }
        Ok(())
    }
}

/// Turns agent events into surface updates. The newest text is held back because the
/// final one comes again as the result.
#[derive(Default)]
struct Relay {
    pending: Option<(String, bool)>,
}

impl Relay {
    fn on(&mut self, event: Event) -> Vec<Update> {
        let mut out = Vec::new();
        match event {
            Event::Text { text, nested } => {
                out.extend(self.take());
                self.pending = Some((text, nested));
            }
            Event::Tool {
                name,
                input,
                nested,
            } => {
                out.extend(self.take());
                out.push(Update::Tool {
                    name,
                    input,
                    nested,
                });
            }
            Event::Started { .. } | Event::Result { .. } => {}
        }
        out
    }

    /// Releases held text unless it is the result about to be posted.
    fn flush_except(&mut self, result: &str) -> Vec<Update> {
        match self.pending.take() {
            Some((text, _)) if text == result => Vec::new(),
            Some((text, nested)) => vec![Update::Thought { text, nested }],
            None => Vec::new(),
        }
    }

    fn take(&mut self) -> Option<Update> {
        self.pending
            .take()
            .map(|(text, nested)| Update::Thought { text, nested })
    }
}

/// New complete lines of a progress file that tools in the agent's process tree append to.
struct ProgressTail {
    path: PathBuf,
    offset: usize,
}

impl ProgressTail {
    fn create(path: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, "")?;
        Ok(Self { path, offset: 0 })
    }

    fn read_new(&mut self) -> Vec<String> {
        let Ok(bytes) = std::fs::read(&self.path) else {
            return Vec::new();
        };
        let Some(end) = bytes.iter().rposition(|&b| b == b'\n').map(|i| i + 1) else {
            return Vec::new();
        };
        if end <= self.offset {
            return Vec::new();
        }
        let lines = String::from_utf8_lossy(&bytes[self.offset..end])
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        self.offset = end;
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn relay_holds_back_the_final_text() {
        let mut relay = Relay::default();
        let mut out = Vec::new();
        out.extend(relay.on(Event::Text {
            text: "Looking.".into(),
            nested: false,
        }));
        out.extend(relay.on(Event::Tool {
            name: "Bash".into(),
            input: json!({}),
            nested: true,
        }));
        out.extend(relay.on(Event::Text {
            text: "Done.".into(),
            nested: false,
        }));
        out.extend(relay.flush_except("Done."));
        assert_eq!(
            out,
            vec![
                Update::Thought {
                    text: "Looking.".into(),
                    nested: false
                },
                Update::Tool {
                    name: "Bash".into(),
                    input: json!({}),
                    nested: true
                },
            ]
        );
    }

    #[test]
    fn progress_tail_returns_only_complete_new_lines() {
        let path = std::env::temp_dir().join(format!("mothership-progress-{}", std::process::id()));
        let mut tail = ProgressTail::create(path.clone()).unwrap();
        std::fs::write(&path, "phase: build\nhalf").unwrap();
        assert_eq!(tail.read_new(), vec!["phase: build"]);
        std::fs::write(&path, "phase: build\nhalf done\n").unwrap();
        assert_eq!(tail.read_new(), vec!["half done"]);
        assert_eq!(tail.read_new(), Vec::<String>::new());
        std::fs::remove_file(&path).unwrap();
    }
}
