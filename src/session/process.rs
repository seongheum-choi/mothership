//! One agent process of a turn: its events relayed to the surface, prompts that arrive while it
//! works fed to it, and whether it ended on Claude Code's usage limit.

use super::{
    Msg, Outcome, Surface, Update,
    background::{Background, Next},
    relay::{ProgressTail, Relay},
    turn::Turn,
};
use crate::agent::{Agent, Event, Launch, Runner, limit};
use anyhow::{Result, bail};
use std::time::Duration;
use tokio::sync::mpsc;

/// Where an agent that never starts is explained.
const START_HELP: &str =
    "https://github.com/seongheum-choi/mothership#agent-does-not-start-macos-privacy-prompts";

/// How an agent process ended without an error.
pub(super) enum Ended {
    /// It answered or was stopped, and the surface was told.
    Done,
    /// Claude Code ended the turn on the account's usage limit and the turn can move to Codex,
    /// so nothing was reported: its prompts are still unanswered.
    Limited { resets_at: Option<u64> },
}

impl<S: Surface> Turn<'_, S> {
    /// Runs one agent process on `prompt`. `tickets` holds prompts it is answering; on error the
    /// caller fails whatever is left. Prompts it took mid-turn are added to `prompt`; those that
    /// arrive after its input closed go to `queued`.
    pub(super) async fn drive(
        &self,
        launch: Launch,
        prompt: &mut String,
        tickets: &mut Vec<S::Ticket>,
        rx: &mut mpsc::UnboundedReceiver<Msg<S::Ticket>>,
        queued: &mut Vec<(String, S::Ticket)>,
        progress: &mut ProgressTail,
    ) -> Result<Ended> {
        let (app, surface, key) = (self.app, &self.reg.surface, self.key);
        let runner = launch.runner;
        let pinned = launch.pinned;
        let can_switch = runner == Runner::Claude && !pinned && app.codex_installed();
        let mut agent = Agent::spawn(&app.cfg.agent, launch)?;
        agent.send(prompt).await?;
        let mut relay = Relay::default();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        // An agent blocked before its first output (a macOS privacy dialog under launchd) would
        // otherwise hold the session silently forever.
        let start_timeout = app.cfg.agent.start_timeout;
        let start_deadline = tokio::time::sleep(start_timeout);
        tokio::pin!(start_deadline);
        let mut started = false;
        let mut background = Background::new(app.cfg.agent.background_timeout);
        let mut limited = None;
        let mut switching = false;
        loop {
            tokio::select! {
                events = agent.next() => {
                    let Some(events) = events? else { break };
                    self.reg.note(key, |a| a.last_event = crate::store::now_secs());
                    for event in events {
                        match event {
                            Event::Started { session_id } => {
                                started = true;
                                app.store.update(|s| {
                                    s.sessions.entry(key.to_string()).or_default().set_resume(runner, session_id);
                                });
                            }
                            Event::Limited { resets_at } => {
                                if let Some(at) = resets_at {
                                    app.store.update(|s| {
                                        s.claude_resets_at = Some(limit::later(s.claude_resets_at, at));
                                    });
                                }
                                limited = Some(resets_at);
                            }
                            Event::Result { is_error: true, .. } if can_switch && limited.is_some() => {
                                agent.close_input();
                                switching = true;
                            }
                            Event::Background { live } => {
                                background.set_live(live);
                                self.reg.note(key, |a| a.background = live);
                            }
                            Event::Result { text, is_error } => {
                                for update in relay.flush_except(&text) {
                                    surface.update(app, key, update).await;
                                }
                                self.answered(&mut agent, &mut background).await;
                                let outcome = outcome(text, is_error, pinned && limited.is_some());
                                surface.finish(app, key, std::mem::take(tickets), outcome).await;
                            }
                            Event::Usage(usage) => tracing::info!("[{key}] {runner} usage: {usage}"),
                            event => {
                                for update in relay.on(event) {
                                    surface.update(app, key, update).await;
                                }
                            }
                        }
                    }
                }
                Some(msg) = rx.recv() => {
                    if self.on_msg(&mut agent, msg, prompt, tickets, queued).await? {
                        return Ok(Ended::Done);
                    }
                }
                () = tokio::time::sleep_until(background.deadline()), if background.waiting() => {
                    agent.close_input();
                    let text = background.give_up();
                    tracing::warn!("[{key}] {text}");
                    surface.update(app, key, Update::Thought { text, nested: false }).await;
                }
                () = &mut start_deadline, if !started => {
                    agent.kill().await;
                    return Err(not_started(runner, start_timeout));
                }
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
        if let (true, Some(resets_at)) = (switching, limited) {
            return Ok(Ended::Limited { resets_at });
        }
        if !tickets.is_empty() {
            bail!("{runner} exited ({status}) without answering");
        }
        Ok(Ended::Done)
    }

    /// After an answer: closes the agent's input, or keeps it open for the background tasks it
    /// still runs and says so the first time.
    async fn answered(&self, agent: &mut Agent, background: &mut Background) {
        match background.answered() {
            Next::Close => agent.close_input(),
            Next::Wait(text) => {
                tracing::info!("[{}] {text}", self.key);
                let note = Update::Thought {
                    text,
                    nested: false,
                };
                self.reg.surface.update(self.app, self.key, note).await;
            }
            Next::KeepWaiting => {}
        }
    }

    /// Passes a prompt that arrives mid-turn to the agent, or queues it once input closed. A stop
    /// kills the agent and returns true.
    async fn on_msg(
        &self,
        agent: &mut Agent,
        msg: Msg<S::Ticket>,
        prompt: &mut String,
        tickets: &mut Vec<S::Ticket>,
        queued: &mut Vec<(String, S::Ticket)>,
    ) -> Result<bool> {
        let (app, surface, key) = (self.app, &self.reg.surface, self.key);
        match msg {
            Msg::Prompt { text, ticket } if agent.accepts_input() => {
                agent.send(&text).await?;
                prompt.push_str("\n\n");
                prompt.push_str(&text);
                tickets.push(ticket);
                surface.update(app, key, Update::Noted).await;
            }
            Msg::Prompt { text, ticket } => queued.push((text, ticket)),
            Msg::Stop(note) => {
                agent.kill().await;
                tickets.extend(queued.drain(..).map(|(_, ticket)| ticket));
                let outcome = Outcome::Stopped(note);
                surface
                    .finish(app, key, std::mem::take(tickets), outcome)
                    .await;
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// How an answer is reported. A failure on the usage limit of a session pinned to Claude says
/// why it did not move to Codex.
fn outcome(text: String, is_error: bool, pinned_at_limit: bool) -> Outcome {
    match (is_error, pinned_at_limit) {
        (true, true) => Outcome::Failed(format!("{text}\n\n{}", limit::PINNED)),
        (true, false) => Outcome::Failed(text),
        (false, _) => Outcome::Reply(text),
    }
}

/// Why a turn whose agent never announced its session failed.
fn not_started(runner: Runner, timeout: Duration) -> anyhow::Error {
    anyhow::anyhow!(
        "The agent did not start within {}s (no {}) and was stopped. Under launchd on macOS this \
         is usually a privacy dialog nobody can answer: see {START_HELP}",
        timeout.as_secs(),
        runner.start_event()
    )
}
