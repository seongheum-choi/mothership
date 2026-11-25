//! What a runner's output says, as runner-neutral [`Event`]s, and the ones both runners make
//! the same way.

use serde_json::Value;

#[derive(Debug, PartialEq)]
pub enum Event {
    Started {
        session_id: String,
    },
    /// `nested` marks output of a subagent rather than the main agent.
    Text {
        text: String,
        nested: bool,
    },
    Tool {
        name: String,
        input: Value,
        nested: bool,
    },
    Result {
        text: String,
        is_error: bool,
    },
    /// How many background tasks (`Agent` calls, background `Bash`) are still running. Claude
    /// Code wakes the agent when one finishes only while its input is open.
    Background {
        live: usize,
    },
    /// Token counts of a finished turn, as the runner reports them.
    Usage(Value),
    /// Comes right before the failed `Result` of a Claude turn that ended on the account's
    /// usage limit, with when the limit resets (Unix seconds) if Claude Code said.
    Limited {
        resets_at: Option<u64>,
    },
}

impl Event {
    /// `Started` with the runner's session `id`, when there is one.
    pub(super) fn started(id: &Value) -> Option<Self> {
        id.as_str().map(|id| Self::Started {
            session_id: id.to_string(),
        })
    }

    /// `Text` of `text` without surrounding whitespace, when that leaves any.
    pub(super) fn text(text: &str, nested: bool) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty()).then(|| Self::Text {
            text: text.to_string(),
            nested,
        })
    }
}
