//! One agent process of a turn: its events relayed to the surface, prompts that arrive while it
//! works fed to it, and whether it ended on Claude Code's usage limit.

use super::{
    Msg, Outcome, Surface, Update,
    relay::{ProgressTail, Relay},
    turn::Turn,
};
use crate::agent::{Agent, Event, Launch, Runner};
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
        let can_switch = runner == Runner::Claude && app.codex_installed();
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
        let mut limited = None;
        let mut switching = false;
        loop {
            tokio::select! {
                events = agent.next() => {
                    let Some(events) = events? else { break };
                    for event in events {
                        match event {
                            Event::Started { session_id } => {
                                started = true;
                                app.store.update(|s| {
                                    s.sessions.entry(key.to_string()).or_default().set_resume(runner, session_id);
                                });
                            }
                            Event::Limited { resets_at } => {
                                if resets_at.is_some() {
                                    app.store.update(|s| s.claude_resets_at = resets_at);
                                }
                                limited = Some(resets_at);
                            }
                            Event::Result { is_error: true, .. } if can_switch && limited.is_some() => {
                                agent.close_input();
                                switching = true;
                            }
                            Event::Result { text, is_error } => {
                                agent.close_input();
                                for update in relay.flush_except(&text) {
                                    surface.update(app, key, update).await;
                                }
                                let outcome = if is_error { Outcome::Failed(text) } else { Outcome::Reply(text) };
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
                Some(msg) = rx.recv() => match msg {
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
                        surface.finish(app, key, std::mem::take(tickets), Outcome::Stopped(note)).await;
                        return Ok(Ended::Done);
                    }
                },
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
