//! The Claude Code CLI as the agent runtime: `claude -p` with stream-json in both directions,
//! so prompts can be added while it works. Output is translated into runner-neutral
//! [`Event`]s; another runtime (Codex, ...) would provide the same.

use crate::config::ClaudeConfig;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
};

/// Everything one turn needs that depends on the session.
pub struct Launch {
    pub cwd: PathBuf,
    pub system_prompt: String,
    pub resume: Option<String>,
    pub permission_mode: String,
    /// Replaces the configured model for this session.
    pub model: Option<String>,
    pub mcp_configs: Vec<PathBuf>,
    pub plugin_dirs: Vec<PathBuf>,
    /// Claude Code settings layered over the user's (permission deny rules).
    pub settings: Value,
    pub env: Vec<(String, String)>,
}

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
}

pub struct Agent {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
}

impl Agent {
    pub fn spawn(claude: &ClaudeConfig, launch: Launch) -> Result<Self> {
        let mut cmd = Command::new(&claude.bin);
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
            launch.model.as_deref().unwrap_or(&claude.model),
            "--fallback-model",
            &claude.fallback_model,
        ])
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
        let mut child = cmd
            .current_dir(&launch.cwd)
            .envs(launch.env)
            .env_remove("CLAUDECODE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {}", claude.bin))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("claude stdout")?;
        Ok(Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
        })
    }

    /// Whether prompts still reach this process. Input closes at the first result, so a
    /// prompt that arrives later starts a new turn instead.
    pub fn accepts_input(&self) -> bool {
        self.stdin.is_some()
    }

    /// Queues a user message. While the agent works, Claude Code folds it into the
    /// current turn at the next step.
    pub async fn send(&mut self, text: &str) -> Result<()> {
        let stdin = self.stdin.as_mut().context("agent input already closed")?;
        let line = json!({ "type": "user", "message": { "role": "user", "content": text } });
        stdin.write_all(format!("{line}\n").as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Lets the process exit once it has answered everything already sent.
    pub fn close_input(&mut self) {
        self.stdin = None;
    }

    /// Events from the next output line; `None` once the process has exited.
    pub async fn next(&mut self) -> Result<Option<Vec<Event>>> {
        Ok(self.lines.next_line().await?.map(|line| parse(&line)))
    }

    /// Kills the agent and everything it started. Claude Code runs each shell command in
    /// its own process group, so the whole tree is collected first: each process is
    /// frozen (SIGSTOP) before its children are listed, so nothing forks past the sweep.
    pub async fn kill(&mut self) {
        if let Some(root) = self.child.id() {
            let mut tree = vec![root.to_string()];
            let mut next = 0;
            while let Some(pid) = tree.get(next).cloned() {
                let _ = Command::new("kill").args(["-STOP", &pid]).status().await;
                if let Ok(out) = Command::new("pgrep").args(["-P", &pid]).output().await {
                    tree.extend(
                        String::from_utf8_lossy(&out.stdout)
                            .split_whitespace()
                            .map(String::from),
                    );
                }
                next += 1;
            }
            let _ = Command::new("kill").arg("-KILL").args(&tree).status().await;
        }
        let _ = self.child.kill().await;
    }

    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        Ok(self.child.wait().await?)
    }
}

/// One stream-json line from `claude -p --output-format stream-json --verbose`.
pub fn parse(line: &str) -> Vec<Event> {
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
