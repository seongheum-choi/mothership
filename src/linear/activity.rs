//! How agent output reads in a Linear session: thoughts, actions and the todo checklist.

use crate::session::Outcome;
use serde_json::{Value, json};

pub(super) fn thought(body: &str) -> Value {
    json!({ "type": "thought", "body": body })
}

/// Subagent output is marked so it reads as part of the step that started it.
pub(super) fn prefixed(text: &str, nested: bool) -> String {
    if nested {
        format!("↪ {text}")
    } else {
        text.to_string()
    }
}

/// `TodoWrite` becomes a persistent checklist; every other tool call is an ephemeral action.
pub(super) fn tool_activity(name: &str, input: &Value) -> (Value, bool) {
    if name.ends_with("TodoWrite") {
        let body = input["todos"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| {
                let content = t["content"].as_str().unwrap_or_default();
                match t["status"].as_str() {
                    Some("completed") => format!("- [x] {content}"),
                    Some("in_progress") => format!("- [ ] {content} (in progress)"),
                    _ => format!("- [ ] {content}"),
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        return (thought(&body), false);
    }
    let parameter = [
        "command",
        "file_path",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
        "skill",
    ]
    .iter()
    .find_map(|k| input[k].as_str().map(String::from))
    .unwrap_or_else(|| input.to_string());
    let parameter: String = parameter.chars().take(300).collect();
    (
        json!({ "type": "action", "action": name, "parameter": parameter }),
        true,
    )
}

/// How a turn ends: the reply or stop note as a response, a failure as an error.
pub(super) fn outcome(outcome: &Outcome) -> Value {
    match outcome {
        Outcome::Reply(text) => json!({ "type": "response", "body": text }),
        Outcome::Failed(text) => json!({ "type": "error", "body": text }),
        Outcome::Stopped(note) => json!({
            "type": "response",
            "body": note.as_deref().unwrap_or("Stopped."),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_write_becomes_a_checklist() {
        let input = json!({"todos":[{"content":"a","status":"completed"},{"content":"b","status":"in_progress"}]});
        assert_eq!(
            tool_activity("TodoWrite", &input),
            (thought("- [x] a\n- [ ] b (in progress)"), false)
        );
        assert_eq!(
            tool_activity("Bash", &json!({"command":"ls"})),
            (
                json!({"type":"action","action":"Bash","parameter":"ls"}),
                true
            )
        );
    }
}
