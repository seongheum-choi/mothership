//! The Codex CLI: `codex exec --json`, one process per turn with the prompt on stdin, resumed
//! by thread id. Flags and event names are those of Codex 0.146 and 0.160.

use super::{Event, Launch};
use serde_json::{Value, json};
use tokio::process::Command;

/// The command line for one turn of `launch`. Claude Code's permission mode, settings, MCP
/// configs and plugins have no Codex counterpart; Codex runs in its own workspace-write sandbox,
/// and `codex exec` never asks for approval.
pub(super) fn command(bin: &str, launch: &Launch) -> Command {
    let mut cmd = Command::new(bin);
    cmd.args(["exec", "--json", "--skip-git-repo-check"])
        .args(["--sandbox", "workspace-write"])
        .arg("--cd")
        .arg(&launch.cwd)
        // Agents push branches and open pull requests.
        .args(["-c", "sandbox_workspace_write.network_access=true"])
        .arg("-c")
        .arg(format!(
            "developer_instructions={}",
            toml_string(&launch.system_prompt)
        ));
    for dir in writable(&launch.settings) {
        cmd.arg("--add-dir").arg(dir);
    }
    if let Some(model) = codex_model(&launch.models.model) {
        cmd.args(["-m", model]);
    }
    if let Some(effort) = &launch.models.effort {
        cmd.arg("-c")
            .arg(format!("model_reasoning_effort={}", toml_string(effort)));
    }
    if let Some(id) = &launch.resume {
        cmd.args(["resume", id]);
    }
    // `-` reads the prompt from stdin up to its end.
    cmd.arg("-");
    cmd
}

/// A TOML basic string: JSON's string escapes are a subset of TOML's.
fn toml_string(text: &str) -> String {
    Value::from(text).to_string()
}

/// The directories Claude Code's sandbox lets Bash write besides the working directory
/// (`SANDBOX_WRITE` and the session's directory, whose `runs/` it may ask mothership in), so
/// Codex's sandbox opens the same.
fn writable(settings: &Value) -> Vec<&str> {
    settings["sandbox"]["filesystem"]["allowWrite"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// `model` unless it names a Claude model (the instance default is `CLAUDE_MODEL`), which
/// Codex does not know; then Codex runs the model its own config picks.
fn codex_model(model: &str) -> Option<&str> {
    const CLAUDE_ALIASES: &[&str] = &["default", "best", "opus", "sonnet", "haiku", "opusplan"];
    let base = model.strip_suffix("[1m]").unwrap_or(model);
    let claude = CLAUDE_ALIASES.contains(&base) || base.starts_with("claude-");
    (!claude && !model.is_empty()).then_some(model)
}

/// The response of a turn that completed without an agent message.
const NO_REPLY: &str = "Codex finished the turn without a reply.";

/// What one `codex exec --json` process said so far. Its reply is the last agent message,
/// sent before `turn.completed`; `error` events may be retried, so only `turn.failed`, or the
/// output ending without a result, fails the turn.
#[derive(Default)]
pub(super) struct Stream {
    reply: Option<String>,
    error: Option<String>,
    finished: bool,
}

impl Stream {
    /// One JSONL line.
    pub(super) fn parse(&mut self, line: &str) -> Vec<Event> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let item = &v["item"];
        let text = || item["text"].as_str().unwrap_or_default().trim().to_string();
        let tool = |name: String, input: Value| Event::Tool {
            name,
            input,
            nested: false,
        };
        match (v["type"].as_str(), item["type"].as_str()) {
            (Some("thread.started"), _) => v["thread_id"]
                .as_str()
                .map(|id| Event::Started {
                    session_id: id.to_string(),
                })
                .into_iter()
                .collect(),
            (Some("item.started"), Some("command_execution")) => {
                vec![tool("Bash".into(), json!({ "command": item["command"] }))]
            }
            (Some("item.started"), Some("mcp_tool_call")) => {
                let (server, name) = (&item["server"], &item["tool"]);
                let name = format!("mcp__{}__{}", str(server), str(name));
                vec![tool(name, item["arguments"].clone())]
            }
            // Codex 0.160 starts a search with an empty `query` and fills it in when the search
            // ends, so the finished item is the one that says what was searched.
            (Some("item.completed"), Some("web_search")) => {
                vec![tool("WebSearch".into(), json!({ "query": item["query"] }))]
            }
            (Some("item.completed"), Some("file_change")) => item["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| {
                    tool(
                        "Edit".into(),
                        json!({ "file_path": c["path"], "kind": c["kind"] }),
                    )
                })
                .collect(),
            (Some("item.completed"), Some(kind @ ("agent_message" | "reasoning"))) => {
                let text = text();
                if kind == "agent_message" {
                    self.reply = Some(text.clone());
                }
                (!text.is_empty())
                    .then_some(Event::Text {
                        text,
                        nested: false,
                    })
                    .into_iter()
                    .collect()
            }
            (Some("turn.completed"), _) => {
                // A turn can end on a command or an edit with nothing said after it.
                let text = self.reply.take().filter(|t| !t.is_empty());
                let text = text.unwrap_or_else(|| NO_REPLY.into());
                let mut events = vec![Event::Usage(v["usage"].clone())];
                events.extend(self.finish(text, false));
                events
            }
            (Some("turn.failed"), _) => {
                let text = v["error"]["message"].as_str().map(String::from);
                let text = text.or_else(|| self.error.take());
                self.finish(text.unwrap_or_else(|| "Codex turn failed".into()), true)
            }
            (Some("error"), _) => {
                self.error = v["message"].as_str().map(String::from);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// The output ended: a turn that never finished fails with the last error Codex reported.
    pub(super) fn end(&mut self) -> Option<Vec<Event>> {
        let error = self.error.take()?;
        let events = self.finish(error, true);
        (!events.is_empty()).then_some(events)
    }

    fn finish(&mut self, text: String, is_error: bool) -> Vec<Event> {
        if std::mem::replace(&mut self.finished, true) {
            return Vec::new();
        }
        vec![Event::Result { text, is_error }]
    }
}

fn str(v: &Value) -> &str {
    v.as_str().unwrap_or("?")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(lines: &[Value]) -> Vec<Event> {
        let mut stream = Stream::default();
        lines
            .iter()
            .flat_map(|l| stream.parse(&l.to_string()))
            .collect()
    }

    #[test]
    fn turns_items_into_events_and_the_last_message_into_the_result() {
        let events = parse_all(&[
            json!({"type":"thread.started","thread_id":"t1"}),
            json!({"type":"turn.started"}),
            json!({"type":"item.completed","item":{"id":"0","type":"reasoning","text":"Thinking."}}),
            json!({"type":"item.started","item":{"id":"1","type":"command_execution","command":"ls","status":"in_progress"}}),
            json!({"type":"item.completed","item":{"id":"1","type":"command_execution","command":"ls","status":"completed"}}),
            json!({"type":"item.started","item":{"id":"2","type":"mcp_tool_call","server":"linear","tool":"get_issue","arguments":{"id":"SH-1"}}}),
            json!({"type":"item.started","item":{"id":"3","type":"web_search","query":"","action":{"type":"other"}}}),
            json!({"type":"item.completed","item":{"id":"3","type":"web_search","query":"rust 1.95","results":[]}}),
            json!({"type":"item.completed","item":{"id":"4","type":"file_change","changes":[{"path":"a.rs","kind":"update"}]}}),
            json!({"type":"item.updated","item":{"id":"5","type":"todo_list","items":[]}}),
            json!({"type":"item.completed","item":{"id":"6","type":"agent_message","text":" Done. "}}),
            json!({"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":1}}),
        ]);
        let text = |t: &str| Event::Text {
            text: t.into(),
            nested: false,
        };
        let tool = |name: &str, input: Value| Event::Tool {
            name: name.into(),
            input,
            nested: false,
        };
        assert_eq!(
            events,
            vec![
                Event::Started {
                    session_id: "t1".into()
                },
                text("Thinking."),
                tool("Bash", json!({"command":"ls"})),
                tool("mcp__linear__get_issue", json!({"id":"SH-1"})),
                tool("WebSearch", json!({"query":"rust 1.95"})),
                tool("Edit", json!({"file_path":"a.rs","kind":"update"})),
                text("Done."),
                Event::Usage(json!({"input_tokens":3,"output_tokens":1})),
                Event::Result {
                    text: "Done.".into(),
                    is_error: false
                },
            ]
        );
    }

    #[test]
    fn fails_on_turn_failed_or_an_error_before_the_end() {
        let failed = Event::Result {
            text: "quota".into(),
            is_error: true,
        };
        let events = parse_all(&[
            json!({"type":"error","message":"retrying"}),
            json!({"type":"turn.failed","error":{"message":"quota"}}),
            json!({"type":"error","message":"late"}),
        ]);
        assert_eq!(events, vec![failed]);

        let mut stream = Stream::default();
        assert_eq!(stream.parse(r#"{"type":"error","message":"quota"}"#), []);
        assert_eq!(
            stream.end(),
            Some(vec![Event::Result {
                text: "quota".into(),
                is_error: true
            }])
        );
        assert_eq!(stream.end(), None);
        assert_eq!(Stream::default().end(), None);
        assert_eq!(Stream::default().parse("not json"), []);
    }

    #[test]
    fn says_so_when_a_turn_completes_without_a_reply() {
        let events = parse_all(&[
            json!({"type":"item.completed","item":{"id":"0","type":"agent_message","text":" "}}),
            json!({"type":"turn.completed","usage":{}}),
        ]);
        assert_eq!(
            events.last(),
            Some(&Event::Result {
                text: NO_REPLY.into(),
                is_error: false
            })
        );
    }

    #[test]
    fn passes_only_codex_models() {
        assert_eq!(codex_model("gpt-5.5"), Some("gpt-5.5"));
        for claude in ["opus", "sonnet[1m]", "claude-opus-5-5", "default", ""] {
            assert_eq!(codex_model(claude), None, "{claude}");
        }
    }

    #[test]
    fn quotes_toml_strings() {
        assert_eq!(toml_string("say \"hi\"\nnow"), r#""say \"hi\"\nnow""#);
    }
}
