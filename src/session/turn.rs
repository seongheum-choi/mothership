//! One conversation's worker: runs agent turns back to back and feeds prompts that arrive
//! mid-turn into the running agent.

use super::{Msg, Outcome, Registry, Surface, Update, process::Ended, relay::ProgressTail};
use crate::{
    agent::{Runner, limit},
    app::App,
};
use anyhow::Result;
use std::sync::Arc;
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

pub(super) struct Turn<'a, S: Surface> {
    pub(super) app: &'a Arc<App>,
    pub(super) reg: &'a Registry<S>,
    pub(super) key: &'a str,
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
        if let Some(text) = launch.note.take() {
            let note = Update::Thought {
                text,
                nested: false,
            };
            surface.update(app, key, note).await;
        }
        let mut progress =
            ProgressTail::create(app.cfg.progress_dir().join(crate::store::file_name(key)))?;
        launch.env.push((
            "MOTHERSHIP_PROGRESS_FILE".into(),
            progress.path.display().to_string(),
        ));

        // A Claude turn that ends on the usage limit runs again on Codex, with every prompt
        // it was given.
        let retry = (launch.runner == Runner::Claude).then(|| launch.clone());
        let mut prompt = prompt.to_string();
        let ended = self
            .drive(launch, &mut prompt, tickets, rx, queued, &mut progress)
            .await?;
        if let (Ended::Limited { resets_at }, Some(mut launch)) = (ended, retry) {
            let text = limit::switched(resets_at);
            tracing::warn!("[{key}] {text}");
            let note = Update::Thought {
                text,
                nested: false,
            };
            surface.update(app, key, note).await;
            launch.runner = Runner::Codex;
            launch.resume = app.store.read(|s| {
                let rec = s.sessions.get(key);
                rec.and_then(|r| r.resume(Runner::Codex))
            });
            self.drive(launch, &mut prompt, tickets, rx, queued, &mut progress)
                .await?;
        }
        Ok(())
    }
}
