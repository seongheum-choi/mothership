//! Gerrit's REST API as the agent's account: the review a `comment-added` event announced.

use super::{
    event::{Account, Event},
    reply::TAG,
};
use crate::{app::App, config::GerritConfig};
use anyhow::{Context, Result, bail};
use serde_json::Value;

/// An inline or patch set level comment of a review.
#[derive(Debug, PartialEq)]
pub(super) struct Comment {
    pub(super) id: String,
    /// The file, or `/PATCHSET_LEVEL` and `/COMMIT_MSG`.
    pub(super) path: String,
    pub(super) line: Option<u64>,
    pub(super) patch_set: u64,
    pub(super) message: String,
}

/// `GET <GERRIT_URL>/a/<path>`, with Gerrit's `)]}'` guard line taken off the JSON.
async fn get(app: &App, cfg: &GerritConfig, path: &str) -> Result<Value> {
    let res = app
        .http
        .get(format!("{}/a/{path}", cfg.url))
        .basic_auth(&cfg.username, Some(&cfg.http_password))
        .send()
        .await
        .with_context(|| format!("GET {path}"))?;
    let status = res.status();
    let text = res.text().await.with_context(|| format!("GET {path}"))?;
    if !status.is_success() {
        bail!("GET {path}: {status}: {}", text.trim());
    }
    let json = text.strip_prefix(")]}'").unwrap_or(&text);
    serde_json::from_str(json).with_context(|| format!("GET {path}: not JSON"))
}

/// The comments of the review `event` announced, or `None` when it is the agent's own.
pub(super) async fn review(
    app: &App,
    cfg: &GerritConfig,
    event: &Event,
) -> Result<Option<Vec<Comment>>> {
    let messages = get(app, cfg, &format!("changes/{}/messages", event.number)).await?;
    let comments = get(app, cfg, &format!("changes/{}/comments", event.number)).await?;
    Ok(select(&messages, &comments, event))
}

/// Finds the event's change message among `messages` (`ChangeMessageInfo`s): the author's
/// newest with the event's text. The review's comments are the author's in `comments` (path to
/// `CommentInfo`s) written when it was: Gerrit stamps a review's message and comments with one
/// time, kept to the second in the message. Without such a message, the event's text stands
/// alone rather than borrow another review's comments.
fn select(messages: &Value, comments: &Value, event: &Event) -> Option<Vec<Comment>> {
    let second = |v: &Value| s(v).get(..19).unwrap_or_default().to_string();
    let message = messages
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| Account::of(&m["author"]).same(&event.author))
        .filter(|m| s(&m["message"]).trim() == event.message.trim())
        .max_by_key(|m| s(&m["date"]));
    let Some(message) = message else {
        return Some(Vec::new());
    };
    if message["tag"] == TAG {
        return None;
    }
    let written = second(&message["date"]);
    let mut found = Vec::new();
    for (path, list) in comments.as_object().into_iter().flatten() {
        for c in list.as_array().into_iter().flatten() {
            if second(&c["updated"]) == written
                && c["tag"] != TAG
                && Account::of(&c["author"]).same(&event.author)
            {
                found.push(Comment {
                    id: s(&c["id"]).to_string(),
                    path: path.clone(),
                    line: c["line"].as_u64(),
                    patch_set: c["patch_set"].as_u64().unwrap_or(event.patch_set),
                    message: s(&c["message"]).trim().to_string(),
                });
            }
        }
    }
    Some(found)
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gerrit::tests::event;
    use serde_json::json;

    #[test]
    fn picks_the_comments_written_with_the_review() {
        let event = event();
        let nate = json!({"_account_id": 1, "name": "Nate", "email": "nate@example.com"});
        let other = json!({"_account_id": 2, "email": "other@example.com"});
        let messages = json!([
            {"id": "m1", "author": nate, "date": "2026-11-07 10:00:00.000000000", "message": "Patch Set 1:\n\n(1 comment)"},
            {"id": "m2", "author": nate, "date": "2026-11-07 10:05:00.000000000", "message": event.message},
            {"id": "m3", "author": nate, "date": "2026-11-07 10:06:00.000000000", "message": "Patch Set 2: Code-Review+1"},
            {"id": "m4", "author": other, "date": "2026-11-07 10:05:00.000000000", "message": event.message},
        ]);
        let comment = |id: &str, author: &Value, updated: &str, line: Option<u64>| json!({"id": id, "author": author, "updated": updated, "line": line, "patch_set": 2, "message": format!(" {id} text ")});
        let comments = json!({
            "src/lib.rs": [
                comment("old", &nate, "2026-11-07 10:00:00.000000000", Some(1)),
                comment("new", &nate, "2026-11-07 10:05:00.123000000", Some(3)),
                comment("theirs", &other, "2026-11-07 10:05:00.000000000", Some(3)),
            ],
            "/PATCHSET_LEVEL": [comment("top", &nate, "2026-11-07 10:05:00.000000000", None)],
        });
        let found = select(&messages, &comments, &event).expect("not the agent's own");
        let ids: Vec<&str> = found.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            ["top", "new"],
            "the exact message wins over a newer one"
        );
        assert_eq!(
            found[1],
            Comment {
                id: "new".into(),
                path: "src/lib.rs".into(),
                line: Some(3),
                patch_set: 2,
                message: "new text".into(),
            }
        );
    }

    #[test]
    fn the_agents_own_review_is_dropped() {
        let event = event();
        let author = json!({"email": "nate@example.com"});
        let tagged = json!([{"author": author, "date": "2026-11-07 10:05:00.000000000", "message": event.message, "tag": TAG}]);
        assert_eq!(select(&tagged, &json!({}), &event), None);
        let none = json!([{"author": {"email": "x@example.com"}, "date": "d", "message": "m"}]);
        assert_eq!(
            select(&none, &json!({}), &event),
            Some(Vec::new()),
            "no message by the author: the event's text alone"
        );
        let other = json!([{"author": author, "date": "2026-11-07 10:06:00.000000000", "message": "Patch Set 3: Code-Review+1"}]);
        let comments = json!({"src/lib.rs": [
            {"id": "c9", "author": author, "updated": "2026-11-07 10:06:00.000000000", "message": "other review"},
        ]});
        assert_eq!(
            select(&other, &comments, &event),
            Some(Vec::new()),
            "another review's message lends it no comments"
        );
    }
}
