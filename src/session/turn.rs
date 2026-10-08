//! One conversation's worker: runs agent turns back to back and feeds prompts that arrive
//! mid-turn into the running agent.

use super::{
    Msg, Outcome, Registry, Surface, Update,
    relay::{ProgressTail, Relay},
};
use crate::{
    agent::{Agent, Event},
    app::App,
};
use anyhow::{Result, bail};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

pub(super) async fn worker<S: Surface>(
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
