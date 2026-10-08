//! The Zulip `Surface`: how a conversation's agent is launched and how its answer is posted.

use super::{ANSWERED, RECEIVED, Ticket, Zulip, client};
use crate::{
    agent::Launch,
    app::App,
    repos::Repo,
    sandbox,
    session::{Outcome, Surface, Update},
    store::file_name,
};
use anyhow::{Context, Result};
use std::sync::Arc;

/// Zulip rejects longer messages (limit 10000).
const MAX_REPLY: usize = 9_500;

fn launch(app: &App, key: &str) -> Result<Launch> {
    let rec = app
        .store
        .read(|s| s.sessions.get(key).cloned())
        .context("unknown conversation")?;
    let workspace = app.cfg.home.join("zulip-workspaces").join(file_name(key));
    std::fs::create_dir_all(&workspace)?;
    let system_prompt = format!(
        "You are {agent}, answering in a Zulip conversation ({location}). Every message you \
         receive is addressed to you, as an @mention or a direct message, and starts with who \
         sent it. Topic messages you have not seen yet arrive in a <zulip_topic_context> block \
         as background, not as separate requests.\n\n\
         Your working directory is scratch space for this conversation, not a repository. \
         These repositories are open to you read-only:\n{repos}\n\
         Do not change code from here. When a request needs code \
         changes, create a Linear issue with clear acceptance criteria and delegate it to \
         yourself (the Linear user the Linear MCP is authenticated as); that starts an issue \
         session that does the work.\n\n\
         Your final reply is posted to Zulip as written, so keep it short. Zulip renders \
         Markdown links, bold, tables and fenced code blocks (give them a language). Do not use \
         headings; put a bold line on its own instead. Mention people as @**Full Name**.",
        agent = app.cfg.agent_name,
        location = rec.title,
        repos = repo_list(&app.cfg.repos),
    );
    let plugin_dirs = app.cfg.plugin_dirs();
    let mut readable = vec![workspace.clone()];
    readable.extend(app.cfg.repos.iter().map(|r| r.path.clone()));
    readable.extend(plugin_dirs.iter().cloned());
    let read_only: Vec<String> = app
        .cfg
        .repos
        .iter()
        .map(|r| format!("Edit(/{}/**)", r.path.display()))
        .collect();
    let mut mcp_configs = vec![crate::linear::mcp_config(app, key)?];
    mcp_configs.extend(app.cfg.mcp_configs.iter().cloned());
    Ok(Launch {
        settings: sandbox::settings(&app.home_dir, &readable, &read_only, &app.cfg.sandbox),
        cwd: workspace,
        system_prompt,
        resume: rec.claude_session_id,
        permission_mode: app.cfg.claude.chat_permission_mode.clone(),
        model: None,
        mcp_configs,
        plugin_dirs,
        env: app.agent_env("zulip"),
    })
}

/// One line per repository for the chat system prompt.
fn repo_list(repos: &[Repo]) -> String {
    repos
        .iter()
        .map(|r| {
            let path = r.path.display();
            if r.git {
                format!(
                    "- `{}`: {path}, a git checkout; run `git -C {path} pull` first when \
                     freshness matters\n",
                    r.name
                )
            } else {
                format!("- `{}`: {path}, a plain directory (not git)\n", r.name)
            }
        })
        .collect()
}

impl Surface for Zulip {
    type Ticket = Ticket;

    fn launch(&self, app: &Arc<App>, key: &str) -> impl Future<Output = Result<Launch>> + Send {
        std::future::ready(launch(app, key))
    }

    /// Chat shows only the answer; the :eyes: reaction already says it is being worked on.
    fn update(
        &self,
        _app: &Arc<App>,
        _key: &str,
        _update: Update,
    ) -> impl Future<Output = ()> + Send {
        std::future::ready(())
    }

    async fn finish(&self, app: &Arc<App>, key: &str, tickets: Vec<Ticket>, outcome: Outcome) {
        let Some(dest) = tickets.last().map(|t| t.dest.clone()) else {
            return;
        };
        let client = client(app);
        let reply = match outcome {
            Outcome::Reply(text) => text,
            Outcome::Failed(text) => format!("Sorry, that failed: {text}"),
            Outcome::Stopped(_) => return,
        };
        let reply: String = reply.chars().take(MAX_REPLY).collect();
        if let Err(e) = client.post_message(&dest, &reply).await {
            tracing::error!("[{key}] zulip reply failed: {e:#}");
            return;
        }
        for ticket in tickets {
            let swapped = async {
                client.react(ticket.message_id, RECEIVED, false).await?;
                client.react(ticket.message_id, ANSWERED, true).await
            };
            if let Err(e) = swapped.await {
                tracing::warn!("[{key}] zulip reaction failed: {e:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_repos_by_kind() {
        let mut vault = Repo::single("/notes/vault".into(), "main".into());
        vault.git = false;
        let repos = [Repo::single("/src/app".into(), "main".into()), vault];
        assert_eq!(
            repo_list(&repos),
            "- `app`: /src/app, a git checkout; run `git -C /src/app pull` first when freshness \
             matters\n- `vault`: /notes/vault, a plain directory (not git)\n"
        );
    }
}
