//! The agent runtime behind a turn: which [`Runner`] it is, its process, the input it takes
//! and its output as runner-neutral [`Event`]s. `claude.rs` speaks Claude Code's protocol,
//! `codex.rs` the Codex CLI's, and `limit.rs` when Claude Code's usage limit moves turns to Codex.

mod claude;
mod codex;
pub mod limit;

use crate::config::AgentConfig;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, path::PathBuf, process::Stdio, str::FromStr};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout},
};

/// The CLI that runs a turn: `AGENT_RUNNER`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runner {
    #[default]
    Claude,
    Codex,
}

impl Runner {
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// The first event of a run, which an agent that never starts does not send.
    pub fn start_event(self) -> &'static str {
        match self {
            Self::Claude => "`system:init` from Claude Code",
            Self::Codex => "`thread.started` from Codex",
        }
    }
}

impl fmt::Display for Runner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Runner {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            _ => Err(format!("{s:?} is not a runner; use claude or codex")),
        }
    }
}

/// Everything one turn needs that depends on the session.
#[derive(Clone)]
pub struct Launch {
    pub runner: Runner,
    /// A message chose the runner, so a turn on Claude Code that hits its usage limit fails
    /// rather than moving to Codex.
    pub pinned: bool,
    pub cwd: PathBuf,
    pub system_prompt: String,
    /// The runner's session to continue: a Claude session id or a Codex thread id.
    pub resume: Option<String>,
    pub permission_mode: String,
    pub models: Models,
    /// Said as the turn's first thought: the runner, model or effort changed since the last
    /// turn, or Claude Code's usage limit moved it to Codex.
    pub note: Option<String>,
    pub mcp_configs: Vec<PathBuf>,
    pub plugin_dirs: Vec<PathBuf>,
    /// Claude Code settings layered over the user's (permission deny rules).
    pub settings: Value,
    /// The same rules for what mothership runs on the session's behalf, with Codex's own
    /// paths readable: see [`crate::host`].
    pub host_settings: Value,
    /// The repository's `commands`, which the session may ask mothership to run.
    pub commands: std::collections::BTreeMap<String, String>,
    pub env: Vec<(String, String)>,
}

/// `--model`, `--fallback-model` and `--effort` for one turn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Models {
    pub model: String,
    pub fallback: String,
    /// `None` leaves the flag off, so Claude Code picks.
    pub effort: Option<String>,
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
    /// Token counts of a finished turn, as the runner reports them.
    Usage(Value),
    /// Comes right before the failed `Result` of a Claude turn that ended on the account's
    /// usage limit, with when the limit resets (Unix seconds) if Claude Code said.
    Limited {
        resets_at: Option<u64>,
    },
}

/// How a runner's output is read: Codex keeps state across lines.
enum Output {
    Claude(claude::Stream),
    Codex(codex::Stream),
}

pub struct Agent {
    output: Output,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
}

impl Agent {
    pub fn spawn(cfg: &AgentConfig, launch: Launch) -> Result<Self> {
        let (mut cmd, output) = match launch.runner {
            Runner::Claude => (
                claude::command(&cfg.claude_bin, &launch),
                Output::Claude(claude::Stream::default()),
            ),
            Runner::Codex => (
                codex::command(&cfg.codex_bin, &launch),
                Output::Codex(codex::Stream::default()),
            ),
        };
        let bin = cmd.as_std().get_program().to_string_lossy().into_owned();
        let mut child = cmd
            .current_dir(&launch.cwd)
            .envs(launch.env)
            .env_remove("CLAUDECODE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {bin}"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("agent stdout")?;
        Ok(Self {
            output,
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
        })
    }

    /// Whether prompts still reach this process. Input closes at the first result, and Codex
    /// takes only the first prompt, so a prompt that arrives later starts a new turn instead.
    pub fn accepts_input(&self) -> bool {
        self.stdin.is_some()
    }

    /// Queues a user message. While the agent works, Claude Code folds it into the
    /// current turn at the next step; Codex reads its prompt to the end of input.
    pub async fn send(&mut self, text: &str) -> Result<()> {
        let stdin = self.stdin.as_mut().context("agent input already closed")?;
        let input = match self.output {
            Output::Claude(_) => claude::message(text),
            Output::Codex(_) => text.to_string(),
        };
        stdin.write_all(input.as_bytes()).await?;
        stdin.flush().await?;
        if let Output::Codex(_) = self.output {
            self.stdin = None;
        }
        Ok(())
    }

    /// Lets the process exit once it has answered everything already sent.
    pub fn close_input(&mut self) {
        self.stdin = None;
    }

    /// Events from the next output line; `None` once the process has exited.
    pub async fn next(&mut self) -> Result<Option<Vec<Event>>> {
        let line = self.lines.next_line().await?;
        Ok(match (&mut self.output, line) {
            (Output::Claude(stream), line) => line.map(|line| stream.parse(&line)),
            (Output::Codex(stream), Some(line)) => Some(stream.parse(&line)),
            (Output::Codex(stream), None) => stream.end(),
        })
    }

    /// Kills the agent and everything it started: see [`kill_tree`].
    pub async fn kill(&mut self) {
        if let Some(root) = self.child.id() {
            let _ = tokio::task::spawn_blocking(move || kill_tree(root)).await;
        }
        let _ = self.child.kill().await;
    }

    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        Ok(self.child.wait().await?)
    }
}

/// Kills `root` and everything it started. Agents run shell commands in their own process
/// groups, so the whole tree is collected first: each process is frozen (SIGSTOP) before its
/// children are listed, so nothing forks past the sweep.
pub fn kill_tree(root: u32) {
    use std::process::Command;
    let mut tree = vec![root.to_string()];
    let mut next = 0;
    while let Some(pid) = tree.get(next).cloned() {
        let _ = Command::new("kill").args(["-STOP", &pid]).status();
        if let Ok(out) = Command::new("pgrep").args(["-P", &pid]).output() {
            tree.extend(
                String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .map(String::from),
            );
        }
        next += 1;
    }
    let _ = Command::new("kill").arg("-KILL").args(&tree).status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_names_round_trip() {
        for runner in [Runner::Claude, Runner::Codex] {
            assert_eq!(runner.name().parse(), Ok(runner));
        }
        assert!("gpt".parse::<Runner>().is_err());
    }
}
