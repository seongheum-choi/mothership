//! How agent output reads in a Linear session: thoughts, actions, the todo checklist, and the
//! session's plan showing a wait for background work.

use super::Linear;
use crate::{
    app::App,
    session::{Outcome, Wait},
};
use anyhow::Result;
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

/// The session's plan while the agent waits for its background tasks: one step, in progress
/// until the wait ends. Linear replaces the whole plan on each update.
pub(super) fn plan(wait: &Wait) -> Value {
    let status = match wait {
        Wait::Started { .. } => "inProgress",
        Wait::Finished => "completed",
        Wait::Abandoned(_) => "canceled",
    };
    json!([{ "content": wait.note(), "status": status }])
}

impl Linear {
    /// Shows the wait in the session's plan (a technology preview of Linear's), or in an
    /// ephemeral thought when the plan cannot be set.
    pub(super) async fn show_wait(&self, app: &App, key: &str, wait: &Wait) {
        if let Err(e) = self.set_plan(app, key, plan(wait)).await {
            tracing::warn!("[{key}] plan update failed, posting a thought instead: {e:#}");
            self.activity(app, key, thought(&wait.note()), true).await;
        }
    }

    async fn set_plan(&self, app: &App, key: &str, plan: Value) -> Result<()> {
        self.update_session(app, key, json!({ "plan": plan })).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_background_wait_is_one_plan_step() {
        let step = |wait: &Wait| {
            let plan = plan(wait);
            (plan[0]["status"].clone(), plan[0]["content"].clone())
        };
        let started = Wait::Started { tasks: 1 };
        assert_eq!(step(&started), (json!("inProgress"), json!(started.note())));
        assert_eq!(
            step(&Wait::Finished),
            (json!("completed"), json!("Background tasks finished."))
        );
        assert_eq!(
            step(&Wait::Abandoned("Gave up.".into())),
            (json!("canceled"), json!("Gave up."))
        );
        assert_eq!(plan(&Wait::Finished).as_array().map(Vec::len), Some(1));
    }

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
