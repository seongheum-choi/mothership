//! Watching a session's directory for run requests while its turn lasts, and serving them one
//! at a time.

use super::{Host, Request, process, request};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};

/// Serves `host`'s requests one at a time until stopped or dropped; either kills a run in
/// flight.
pub struct Watch(tokio::task::JoinHandle<()>);

impl Watch {
    /// Stops serving and waits until a run in flight is killed, so the workspace is free.
    pub async fn stop(&mut self) {
        self.0.abort();
        let _ = (&mut self.0).await;
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub fn watch(key: String, host: Host) -> Watch {
    Watch(tokio::spawn(async move {
        let mut failed = HashSet::new();
        loop {
            // The agent may replace `runs/` with a link to somewhere mothership should not
            // read requests from or write answers to.
            let real = std::fs::symlink_metadata(&host.runs).is_ok_and(|m| m.is_dir());
            let pending = if real {
                request::pending(&host.runs)
            } else {
                Vec::new()
            };
            for id in pending {
                if failed.contains(&id) {
                    continue;
                }
                let outcome = match request::read_plain(&request::request(&host.runs, &id)) {
                    Ok(text) => match request::parse(&text, &host.sandbox_limits) {
                        Ok(req) => host.serve(&key, &id, &req).await,
                        Err(e) => Err(format!("refused: {e}")),
                    },
                    Err(e) => Err(format!("unreadable request: {e}")),
                };
                if let Err(e) = request::write_answer(&host.runs, &id, outcome) {
                    tracing::warn!("[{key}] cannot answer run {id}: {e}");
                    failed.insert(id);
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }))
}

impl Host {
    async fn serve(&self, key: &str, id: &str, req: &Request) -> Result<Value, String> {
        let what = match req {
            Request::Codex(_) => "Codex run".to_string(),
            Request::Command(name) => format!("`{name}`"),
            Request::Sandbox(_) => "Sandbox run".to_string(),
        };
        self.report(&format!("{what} {id} started"));
        let outcome = match process::RunDir::create(&self.run_root) {
            Ok(dir) => match req {
                Request::Codex(run) => self.codex(&dir.0, run).await,
                Request::Command(name) => {
                    self.command(&dir.0, name, &request::log(&self.runs, id))
                        .await
                }
                Request::Sandbox(run) => self.sandbox(id, run).await,
            },
            Err(e) => Err(format!("cannot make a directory for the run: {e}")),
        };
        match &outcome {
            Ok(_) => self.report(&format!("{what} {id} finished")),
            Err(e) => {
                tracing::warn!("[{key}] {what} {id} failed: {e}");
                self.report(&format!(
                    "{what} {id} failed: {}",
                    e.lines().next().unwrap_or_default()
                ));
            }
        }
        outcome
    }
}
