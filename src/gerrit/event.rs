//! What a Gerrit `comment-added` event says, and whether it may reach an agent.

use crate::config::GerritConfig;
use serde_json::Value;

/// A Gerrit account as events and the REST API describe it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Account {
    pub(super) name: String,
    pub(super) email: String,
    pub(super) username: String,
}

impl Account {
    /// From an event's account attribute or the REST API's `AccountInfo`.
    pub(super) fn of(v: &Value) -> Self {
        let s = |key: &str| v[key].as_str().unwrap_or_default().to_string();
        Self {
            name: s("name"),
            email: s("email"),
            username: s("username"),
        }
    }

    /// Whether both describe one account: the same email, or else the same username. Which
    /// fields Gerrit fills in depends on the caller's permissions, so either may be missing.
    pub(super) fn same(&self, other: &Self) -> bool {
        let both = |a: &str, b: &str| !a.is_empty() && a.eq_ignore_ascii_case(b);
        both(&self.email, &other.email) || both(&self.username, &other.username)
    }

    pub(super) fn label(&self) -> &str {
        [&self.name, &self.username, &self.email]
            .into_iter()
            .find(|s| !s.is_empty())
            .map_or("someone", String::as_str)
    }
}

/// A review posted on a change: a `comment-added` event.
#[derive(Debug, PartialEq)]
pub(super) struct Event {
    pub(super) project: String,
    /// Agents push with the issue key as topic; empty when the change has none.
    pub(super) topic: String,
    pub(super) number: u64,
    pub(super) url: String,
    pub(super) patch_set: u64,
    pub(super) author: Account,
    /// The review's change message (`Patch Set 2: Code-Review-1\n\n(3 comments)\n\n...`).
    pub(super) message: String,
}

impl Event {
    /// Keeps `comment-added` events, in the JSON of `gerrit stream-events` and the webhooks
    /// plugin; every other event is `None`.
    pub(super) fn parse(p: &Value) -> Option<Self> {
        if p["type"] != "comment-added" {
            return None;
        }
        let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
        let change = &p["change"];
        Some(Self {
            project: s(&change["project"]),
            topic: s(&change["topic"]).trim().to_string(),
            number: number(&change["number"])?,
            url: s(&change["url"]),
            patch_set: number(&p["patchSet"]["number"])?,
            author: Account::of(&p["author"]),
            message: s(&p["comment"]),
        })
    }

    /// Why this review must not reach an agent, if it must not. `watched` tells whether the
    /// event's project is the origin of one of this instance's repositories.
    pub(super) fn screen(&self, watched: bool, cfg: &GerritConfig) -> Result<(), &'static str> {
        if !watched {
            return Err("another project");
        }
        if !trusts(cfg, &self.author) {
            return Err("untrusted author");
        }
        if self.topic.is_empty() {
            return Err("change without a topic");
        }
        Ok(())
    }
}

/// Numbers are JSON numbers in current events and strings in old ones.
fn number(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

/// Whether the author's email is one of `GERRIT_TRUSTED_EMAILS`, in any letter case.
fn trusts(cfg: &GerritConfig, author: &Account) -> bool {
    !author.email.is_empty()
        && cfg
            .trusted_emails
            .iter()
            .any(|e| e.eq_ignore_ascii_case(&author.email))
}

/// Whether `text` mentions `name` (`@name`, in any letter case). Gerrit completes a mention
/// to `@name` or `@email`, so `@name@example.com` mentions `name` too, while `@name-2`,
/// `@name_2` and `x@name` do not.
pub(super) fn mentions(text: &str, name: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let needle = format!("@{}", name.to_ascii_lowercase());
    let name_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    lower.match_indices(&needle).any(|(i, _)| {
        let before = lower[..i].chars().next_back();
        let after = lower[i + needle.len()..].chars().next();
        !before.is_some_and(name_char) && !after.is_some_and(name_char)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gerrit::tests::{cfg, comment_added};
    use serde_json::json;

    #[test]
    fn parses_comment_added_only() {
        let event = Event::parse(&comment_added()).expect("comment-added parses");
        assert_eq!(event.project, "team/app");
        assert_eq!((event.number, event.patch_set), (12, 2));
        assert_eq!(event.topic, "SH-8");
        assert_eq!(event.author.label(), "Alice");
        let mut old = comment_added();
        old["change"]["number"] = json!("12");
        old["patchSet"]["number"] = json!("2");
        assert_eq!(Event::parse(&old), Some(event), "numbers as strings");
        let mut other = comment_added();
        other["type"] = json!("patchset-created");
        assert_eq!(Event::parse(&other), None);
    }

    #[test]
    fn screens_project_author_and_topic() {
        let cfg = cfg();
        let event = Event::parse(&comment_added()).expect("comment-added parses");
        assert_eq!(event.screen(true, &cfg), Ok(()));
        assert_eq!(event.screen(false, &cfg), Err("another project"));
        let mut outsider = comment_added();
        outsider["author"]["email"] = json!("mallory@example.com");
        let outsider = Event::parse(&outsider).expect("parses");
        assert_eq!(outsider.screen(true, &cfg), Err("untrusted author"));
        let mut no_email = comment_added();
        no_email["author"] = json!({"username": "Alice@Example.com"});
        let no_email = Event::parse(&no_email).expect("parses");
        assert_eq!(no_email.screen(true, &cfg), Err("untrusted author"));
        let mut no_topic = comment_added();
        no_topic["change"]
            .as_object_mut()
            .expect("change is an object")
            .remove("topic");
        let no_topic = Event::parse(&no_topic).expect("parses");
        assert_eq!(no_topic.screen(true, &cfg), Err("change without a topic"));
    }

    #[test]
    fn mention_needs_the_whole_name() {
        let name = "acme-bot";
        for text in [
            "@acme-bot please fix",
            "Hi @Acme-Bot.",
            "(@acme-bot)",
            "@acme-bot@example.com look",
        ] {
            assert!(mentions(text, name), "{text}");
        }
        for text in ["acme-bot", "@acme-bot-2", "@acme-bot_bot", "x@acme-bot"] {
            assert!(!mentions(text, name), "{text}");
        }
    }

    #[test]
    fn accounts_match_by_email_or_username() {
        let a = |name: &str, email: &str, username: &str| Account {
            name: name.into(),
            email: email.into(),
            username: username.into(),
        };
        assert!(a("", "N@x.com", "").same(&a("Alice", "n@x.com", "")));
        assert!(a("", "", "alice").same(&a("", "n@x.com", "alice")));
        assert!(
            !a("Alice", "", "").same(&a("Alice", "", "")),
            "names repeat"
        );
        assert!(!a("", "", "").same(&a("", "", "")));
        assert_eq!(a("", "", "").label(), "someone");
    }
}
