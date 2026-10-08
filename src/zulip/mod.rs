//! Zulip conversations through an outgoing-webhook bot: an @mention in a channel or a
//! direct message starts or continues a conversation, one per topic (or DM group).

mod api;

use crate::{
    agent::Launch,
    app::App,
    repos::Repo,
    sandbox,
    session::{Outcome, Surface, Update},
    signature,
    store::file_name,
};
use anyhow::{Context, Result};
use api::{Client, Destination};
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use serde_json::{Value, json};
use std::{fmt::Write as _, sync::Arc};

const RECEIVED: &str = "eyes";
const ANSWERED: &str = "check";
/// Zulip prefixes a resolved topic's name with this.
const RESOLVED: &str = "✔ ";
const CONTEXT_MESSAGES: u32 = 50;
/// Zulip rejects longer messages (limit 10000).
const MAX_REPLY: usize = 9_500;

pub struct Zulip;

pub struct Ticket {
    message_id: u64,
    dest: Destination,
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/zulip-webhook", post(webhook))
}

async fn webhook(State(app): State<Arc<App>>, Json(p): Json<Value>) -> (StatusCode, Json<Value>) {
    let ok = (
        StatusCode::OK,
        Json(json!({ "response_not_required": true })),
    );
    let Some(cfg) = &app.cfg.zulip else {
        return (StatusCode::NOT_FOUND, Json(json!({})));
    };
    let token = p["token"].as_str().unwrap_or_default();
    if !signature::constant_time_eq(token.as_bytes(), cfg.webhook_token.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, Json(json!({})));
    }
    let trigger = p["trigger"].as_str().unwrap_or_default();
    if !matches!(trigger, "mention" | "direct_message" | "private_message")
        || p["message"]["id"].as_u64().is_none()
    {
        return ok;
    }
    tokio::spawn(handle(app, p));
    ok
}

async fn handle(app: Arc<App>, p: Value) {
    let Some(zulip) = &app.zulip else { return };
    let message = &p["message"];
    let Some(message_id) = message["id"].as_u64() else {
        return;
    };
    let client = app.zulip_client();
    if let Err(e) = client.react(message_id, RECEIVED, true).await {
        tracing::warn!("zulip reaction failed: {e:#}");
    }
    let (key, dest, location) = conversation(message);
    tracing::info!("[{key}] zulip message {message_id}");

    let mut prompt = String::new();
    if let Destination::Stream { topic, .. } = &dest
        && let Some(channel) = message["display_recipient"].as_str()
    {
        let cursor = app
            .store
            .read(|s| s.sessions.get(&key).and_then(|r| r.cursor));
        match client
            .topic_messages(channel, topic, CONTEXT_MESSAGES)
            .await
        {
            Ok(messages) => prompt.push_str(&topic_context(
                &messages,
                message_id,
                cursor,
                &zulip_bot(&app),
            )),
            Err(e) => tracing::warn!("zulip topic context failed: {e:#}"),
        }
    }
    let _ = write!(
        prompt,
        "From {} ({}):\n{}",
        message["sender_full_name"].as_str().unwrap_or_default(),
        message["sender_email"].as_str().unwrap_or_default(),
        strip_mention(
            p["data"]
                .as_str()
                .or(message["content"].as_str())
                .unwrap_or_default()
        )
    );
    app.store.update(|s| {
        let rec = s.sessions.entry(key.clone()).or_default();
        rec.title = location;
        rec.cursor = Some(message_id);
    });
    zulip.submit(&app, &key, prompt, Ticket { message_id, dest });
}

fn zulip_bot(app: &App) -> String {
    app.cfg
        .zulip
        .as_ref()
        .map(|z| z.bot_email.clone())
        .unwrap_or_default()
}

/// Session key, reply destination, and a human-readable location for a message.
fn conversation(message: &Value) -> (String, Destination, String) {
    if message["type"] == "stream"
        && let Some(stream_id) = message["stream_id"].as_u64()
    {
        let subject = message["subject"].as_str().unwrap_or_default();
        let topic = subject.strip_prefix(RESOLVED).unwrap_or(subject);
        let channel = message["display_recipient"].as_str().unwrap_or("?");
        return (
            format!("zulip:{stream_id}:{topic}"),
            Destination::Stream {
                id: stream_id,
                topic: subject.to_string(),
            },
            format!("#{channel} > {topic}"),
        );
    }
    let mut ids: Vec<u64> = message["display_recipient"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["id"].as_u64())
        .collect();
    if ids.is_empty() {
        ids.extend(message["sender_id"].as_u64());
    }
    ids.sort_unstable();
    let joined = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
    (
        format!("zulip:dm:{joined}"),
        Destination::Private(ids),
        "a direct message".into(),
    )
}

/// Topic messages the agent has not seen yet, as background for the new message.
fn topic_context(messages: &[Value], current: u64, cursor: Option<u64>, bot_email: &str) -> String {
    let unseen: Vec<&Value> = messages
        .iter()
        .filter(|m| {
            let id = m["id"].as_u64().unwrap_or_default();
            id != current && cursor.is_none_or(|c| id > c && m["sender_email"] != bot_email)
        })
        .collect();
    if unseen.is_empty() {
        return String::new();
    }
    let mut out = String::from("<zulip_topic_context>\n");
    for m in unseen {
        let author = if m["sender_email"] == bot_email {
            "you"
        } else {
            m["sender_full_name"].as_str().unwrap_or_default()
        };
        let _ = writeln!(
            out,
            "<message author=\"{author}\" id=\"{}\">\n{}\n</message>",
            m["id"],
            m["content"].as_str().unwrap_or_default()
        );
    }
    out.push_str("</zulip_topic_context>\n\n");
    out
}

/// Drops a leading `@**Name**` / `@_**Name|123**` mention of the bot.
fn strip_mention(text: &str) -> &str {
    let text = text.trim_start();
    let Some(rest) = text
        .strip_prefix("@_**")
        .or_else(|| text.strip_prefix("@**"))
    else {
        return text.trim();
    };
    rest.find("**").map_or(text, |end| &rest[end + 2..]).trim()
}

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
        settings: sandbox::settings(&app.home_dir, &readable, &read_only),
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
        let client = app.zulip_client();
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

impl App {
    fn zulip_client(&self) -> Client<'_> {
        Client {
            http: &self.http,
            cfg: self
                .cfg
                .zulip
                .as_ref()
                .expect("zulip routes run only when configured"),
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

    #[test]
    fn strips_leading_mention() {
        assert_eq!(strip_mention("@**Bot** hi there"), "hi there");
        assert_eq!(strip_mention(" @_**Bot|42** hi"), "hi");
        assert_eq!(strip_mention("hi @**Bot**"), "hi @**Bot**");
    }

    #[test]
    fn keys_topics_and_dms() {
        let stream =
            json!({"type":"stream","stream_id":7,"subject":"✔ deploy","display_recipient":"ops"});
        let (key, dest, location) = conversation(&stream);
        assert_eq!(key, "zulip:7:deploy");
        assert_eq!(
            dest,
            Destination::Stream {
                id: 7,
                topic: "✔ deploy".into()
            }
        );
        assert_eq!(location, "#ops > deploy");

        let dm = json!({"type":"private","display_recipient":[{"id":9},{"id":3}],"sender_id":3});
        let (key, dest, _) = conversation(&dm);
        assert_eq!(key, "zulip:dm:3,9");
        assert_eq!(dest, Destination::Private(vec![3, 9]));
    }

    #[test]
    fn context_shows_only_unseen_messages() {
        let messages = vec![
            json!({"id":1,"sender_email":"a@x","sender_full_name":"A","content":"old"}),
            json!({"id":2,"sender_email":"bot@x","sender_full_name":"Bot","content":"mine"}),
            json!({"id":3,"sender_email":"b@x","sender_full_name":"B","content":"new"}),
            json!({"id":4,"sender_email":"a@x","sender_full_name":"A","content":"current"}),
        ];
        let ctx = topic_context(&messages, 4, Some(1), "bot@x");
        assert_eq!(
            ctx,
            "<zulip_topic_context>\n<message author=\"B\" id=\"3\">\nnew\n</message>\n</zulip_topic_context>\n\n"
        );
        assert!(topic_context(&messages, 4, None, "bot@x").contains("author=\"you\""));
    }
}
