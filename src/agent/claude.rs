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

/// One stream-json line from `claude -p --output-format stream-json --verbose`.
pub(super) fn parse(line: &str) -> Vec<Event> {
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
        Some("assistant") => v["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|block| match block["type"].as_str() {
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
            })
            .collect(),
        Some("result") => {
            let text = v["result"].as_str().unwrap_or_default().trim().to_string();
            let is_error = v["is_error"] == true || v["subtype"] != "success";
            let text = if is_error && text.is_empty() {
                format!("Claude run failed: {}", v["subtype"])
            } else {
                text
            };
            vec![Event::Result { text, is_error }]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
