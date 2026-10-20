//! The Claude Code CLI: `claude -p` with stream-json in both directions, so prompts can be added
//! while it works.

use super::{Event, Launch};
use serde_json::{Value, json};
use tokio::process::Command;

/// The command line for one turn of `launch`.
pub(super) fn command(bin: &str, launch: &Launch) -> Command {
    let mut cmd = Command::new(bin);
    cmd.args([
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
    ])
    .args([
        "--permission-mode",
        &launch.permission_mode,
        "--strict-mcp-config",
    ])
    .args([
        "--model",
        &launch.models.model,
        "--fallback-model",
        &launch.models.fallback,
    ])
    .args(
        launch
            .models
            .effort
            .iter()
            .flat_map(|e| ["--effort", e.as_str()]),
    )
    .arg("--settings")
    .arg(launch.settings.to_string())
    .arg("--append-system-prompt")
    .arg(&launch.system_prompt);
    for config in &launch.mcp_configs {
        cmd.arg("--mcp-config").arg(config);
    }
    for dir in &launch.plugin_dirs {
        cmd.arg("--plugin-dir").arg(dir);
    }
    if let Some(id) = &launch.resume {
        cmd.args(["--resume", id]);
    }
    cmd
}

/// A user message on stdin. While the agent works, Claude Code folds it into the current turn
/// at the next step.
pub(super) fn message(text: &str) -> String {
    let line = json!({ "type": "user", "message": { "role": "user", "content": text } });
    format!("{line}\n")
}

/// The usage-limit refusal of Claude Code 2.1.296: an `assistant` message with
/// `is_api_error_message`, `api_error` and `api_error_params.rate_limit_info.resetsAt` (Unix
/// seconds), then a `result` whose `api_error` names the error that ended the turn.
const USAGE_LIMIT: &str = "usage_limit_reached";

/// What `claude -p --output-format stream-json --verbose` said so far: the reset time of the
/// latest usage-limit refusal, which only counts if the turn ends on it. Claude Code retries and
/// tries `--fallback-model` first, so a refusal it recovered from fails nothing.
#[derive(Default)]
pub(super) struct Stream {
    resets_at: Option<u64>,
}

impl Stream {
    /// One stream-json line.
    pub(super) fn parse(&mut self, line: &str) -> Vec<Event> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let nested = v["parent_tool_use_id"].is_string();
        match v["type"].as_str() {
            Some("system") if v["subtype"] == "init" => v["session_id"]
                .as_str()
                .map(|id| Event::Started {
                    session_id: id.to_string(),
                })
                .into_iter()
                .collect(),
            Some("system") if v["subtype"] == "background_tasks_changed" => {
                vec![Event::Background {
                    live: v["tasks"].as_array().map_or(0, Vec::len),
                }]
            }
            Some("assistant") => {
                if v["api_error"] == USAGE_LIMIT {
                    let info = &v["api_error_params"]["rate_limit_info"];
                    self.resets_at = info["resetsAt"].as_u64();
                }
                v["message"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|block| content(block, nested))
                    .collect()
            }
            Some("result") => {
                let text = v["result"].as_str().unwrap_or_default().trim().to_string();
                let is_error = v["is_error"] == true || v["subtype"] != "success";
                let text = if is_error && text.is_empty() {
                    format!("Claude run failed: {}", v["subtype"])
                } else {
                    text
                };
                let limited = (is_error && v["api_error"] == USAGE_LIMIT).then(|| Event::Limited {
                    resets_at: self.resets_at.take(),
                });
                limited
                    .into_iter()
                    .chain([Event::Result { text, is_error }])
                    .collect()
            }
            _ => Vec::new(),
        }
    }
}

/// One block of an assistant message.
fn content(block: &Value, nested: bool) -> Option<Event> {
    match block["type"].as_str() {
        Some("text") => {
            let text = block["text"].as_str().unwrap_or_default().trim();
            (!text.is_empty()).then(|| Event::Text {
                text: text.to_string(),
                nested,
            })
        }
        Some("tool_use") => Some(Event::Tool {
            name: block["name"].as_str().unwrap_or("Tool").to_string(),
            input: block["input"].clone(),
            nested,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Vec<Event> {
        Stream::default().parse(line)
    }

    #[test]
    fn parses_stream_json() {
        assert_eq!(
            parse(r#"{"type":"system","subtype":"init","session_id":"s1"}"#),
            vec![Event::Started {
                session_id: "s1".into()
            }]
        );
        assert_eq!(
            parse(
                r#"{"type":"assistant","parent_tool_use_id":"t0","message":{"content":[{"type":"text","text":" hi "},{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#
            ),
            vec![
                Event::Text {
                    text: "hi".into(),
                    nested: true
                },
                Event::Tool {
                    name: "Bash".into(),
                    input: json!({"command":"ls"}),
                    nested: true
                },
            ]
        );
        assert_eq!(
            parse(r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#),
            vec![Event::Result {
                text: "Claude run failed: \"error_max_turns\"".into(),
                is_error: true
            }]
        );
        assert_eq!(parse("not json"), vec![]);
        assert_eq!(
            parse(
                r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a1","task_type":"local_agent"},{"task_id":"b2","task_type":"local_bash","parent_task_id":"a1"}]}"#
            ),
            vec![Event::Background { live: 2 }]
        );
        assert_eq!(
            parse(r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#),
            vec![Event::Background { live: 0 }]
        );
    }

    /// The lines Claude Code 2.1.296 writes when the account's usage limit refuses the turn.
    const REFUSAL: &str = r#"{"type":"assistant","error":"rate_limit","is_api_error_message":true,"api_error_status":429,"api_error":"usage_limit_reached","api_error_params":{"rate_limit_info":{"status":"rejected","isUsingOverage":false,"rateLimitType":"five_hour","resetsAt":1791900300}},"parent_tool_use_id":null,"message":{"content":[{"type":"text","text":"You've hit your limit"}]}}"#;
    const ENDED: &str = r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":429,"api_error":"usage_limit_reached","result":"You've hit your limit"}"#;

    #[test]
    fn reports_a_turn_that_ended_on_the_usage_limit() {
        let mut stream = Stream::default();
        let mut events = stream.parse(REFUSAL);
        events.extend(stream.parse(ENDED));
        let failed = Event::Result {
            text: "You've hit your limit".into(),
            is_error: true,
        };
        assert_eq!(
            events,
            vec![
                Event::Text {
                    text: "You've hit your limit".into(),
                    nested: false
                },
                Event::Limited {
                    resets_at: Some(1_791_900_300)
                },
                failed,
            ]
        );
        // Without the refusal's reset time, the limit still ends the turn.
        assert_eq!(
            Stream::default().parse(ENDED)[0],
            Event::Limited { resets_at: None }
        );
    }

    #[test]
    fn ignores_a_refusal_the_turn_recovered_from() {
        let mut stream = Stream::default();
        stream.parse(REFUSAL);
        assert_eq!(
            stream.parse(r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#),
            vec![Event::Result {
                text: "ok".into(),
                is_error: false
            }]
        );
        let other = r#"{"type":"result","subtype":"success","is_error":true,"api_error":"max_output_tokens","result":"x"}"#;
        assert_eq!(Stream::default().parse(other).len(), 1);
    }
}
