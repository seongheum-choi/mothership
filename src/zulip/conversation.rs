//! Which conversation a Zulip message belongs to, and what of it goes into the prompt.

use super::api::Destination;
use serde_json::Value;
use std::fmt::Write as _;

/// Zulip prefixes a resolved topic's name with this.
const RESOLVED: &str = "✔ ";

/// Session key, reply destination, and a human-readable location for a message.
pub(super) fn conversation(message: &Value) -> (String, Destination, String) {
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
pub(super) fn topic_context(
    messages: &[Value],
    current: u64,
    cursor: Option<u64>,
    bot_email: &str,
) -> String {
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
pub(super) fn strip_mention(text: &str) -> &str {
    let text = text.trim_start();
    let Some(rest) = text
        .strip_prefix("@_**")
        .or_else(|| text.strip_prefix("@**"))
    else {
        return text.trim();
    };
    rest.find("**").map_or(text, |end| &rest[end + 2..]).trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
