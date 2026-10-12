//! The agent runtime behind a turn: its process, the input it takes and its output as
//! runner-neutral [`Event`]s. `claude.rs` speaks Claude Code's protocol.

mod claude;

use crate::config::ClaudeConfig;
use anyhow::{Context, Result};
use serde_json::Value;
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
    pub models: Models,
    /// Said as the turn's first thought: the model or effort changed since the last turn.
    pub note: Option<String>,
    pub mcp_configs: Vec<PathBuf>,
    pub plugin_dirs: Vec<PathBuf>,
    /// Claude Code settings layered over the user's (permission deny rules).
    pub settings: Value,
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
}

pub struct Agent {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
}

impl Agent {
    pub fn spawn(claude: &ClaudeConfig, launch: Launch) -> Result<Self> {
        let mut cmd = claude::command(&claude.bin, &launch);
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
        stdin.write_all(claude::message(text).as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Lets the process exit once it has answered everything already sent.
    pub fn close_input(&mut self) {
        self.stdin = None;
    }

    /// Events from the next output line; `None` once the process has exited.
    pub async fn next(&mut self) -> Result<Option<Vec<Event>>> {
        Ok(self
            .lines
            .next_line()
            .await?
            .map(|line| claude::parse(&line)))
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
