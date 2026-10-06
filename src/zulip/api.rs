//! The few Zulip REST calls a chat bot needs.

use crate::config::ZulipConfig;
use anyhow::{Result, bail};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq)]
pub enum Destination {
    Stream { id: u64, topic: String },
    Private(Vec<u64>),
}

pub struct Client<'a> {
    pub http: &'a reqwest::Client,
    pub cfg: &'a ZulipConfig,
}

impl Client<'_> {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}/api/v1{path}", self.cfg.site))
            .basic_auth(&self.cfg.bot_email, Some(&self.cfg.api_key))
    }

    async fn send(req: reqwest::RequestBuilder) -> Result<Value> {
        let res = req.send().await?;
        let status = res.status();
        let body: Value = res.json().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Zulip: HTTP {status}: {}", body["msg"]);
        }
        Ok(body)
    }

    pub async fn post_message(&self, dest: &Destination, content: &str) -> Result<()> {
        let form = match dest {
            Destination::Stream { id, topic } => vec![
                ("type", "stream".to_string()),
                ("to", id.to_string()),
                ("topic", topic.clone()),
                ("content", content.to_string()),
            ],
            Destination::Private(ids) => vec![
                ("type", "private".to_string()),
                ("to", json!(ids).to_string()),
                ("content", content.to_string()),
            ],
        };
        Self::send(self.request(reqwest::Method::POST, "/messages").form(&form)).await?;
        Ok(())
    }

    pub async fn react(&self, message_id: u64, emoji: &str, add: bool) -> Result<()> {
        let path = format!("/messages/{message_id}/reactions");
        let req = if add {
            self.request(reqwest::Method::POST, &path)
                .form(&[("emoji_name", emoji)])
        } else {
            self.request(reqwest::Method::DELETE, &path)
                .query(&[("emoji_name", emoji)])
        };
        Self::send(req).await?;
        Ok(())
    }

    /// The newest `limit` messages of a topic, oldest first.
    pub async fn topic_messages(
        &self,
        channel: &str,
        topic: &str,
        limit: u32,
    ) -> Result<Vec<Value>> {
        let narrow = json!([
            { "operator": "channel", "operand": channel },
            { "operator": "topic", "operand": topic },
        ]);
        let req = self.request(reqwest::Method::GET, "/messages").query(&[
            ("narrow", narrow.to_string()),
            ("anchor", "newest".into()),
            ("num_before", limit.to_string()),
            ("num_after", "0".into()),
            ("apply_markdown", "false".into()),
        ]);
        let body = Self::send(req).await?;
        Ok(body["messages"].as_array().cloned().unwrap_or_default())
    }
}
