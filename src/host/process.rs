//! Running a confined process for a session: under `HOST_SANDBOX` with the run's profile, only
//! its own environment, in a process group of its own, with a time limit, and nothing it
//! started left running afterwards.

use super::{Host, TIMEOUT, request};
use std::{
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

/// How much of a run's output is kept, from its end.
const OUTPUT_CAP: usize = 1 << 20;
/// How long a run's output is still read after it exited: a process it left behind may hold
/// the pipe open.
const DRAIN: Duration = Duration::from_secs(5);

/// What a run printed, its ends kept.
pub(super) struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Host {
    /// Runs `program` with `args` in the workspace under `HOST_SANDBOX` and `profile`, with
    /// `input` on stdin, only [`super::env`]'s environment and `TMPDIR` in `run_dir`, until it
    /// exits or [`TIMEOUT`] passes. Its process group is killed either way.
    pub(super) async fn confined(
        &self,
        profile: String,
        run_dir: &Path,
        program: &str,
        args: &[String],
        input: Option<&str>,
    ) -> Result<Output, String> {
        let tmp = run_dir.join("tmp");
        std::fs::create_dir(&tmp).map_err(|e| e.to_string())?;
        let mut child = Command::new(&self.sandbox)
            .args(["-p".to_string(), profile, program.to_string()])
            .args(args)
            .current_dir(&self.cwd)
            .env_clear()
            .envs(self.env.iter().cloned())
            .env("TMPDIR", &tmp)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            // Not kill_on_drop: killing the root first would orphan its children before
            // `Group` lists them.
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", self.sandbox))?;
        let mut group = Group(child.id());
        let stdout = tokio::spawn(keep_end(child.stdout.take()));
        let stderr = tokio::spawn(keep_end(child.stderr.take()));
        // Written alongside the wait, so a process that never reads it cannot outlast TIMEOUT.
        let stdin = child.stdin.take();
        let input = input.map(str::to_owned);
        let writer = tokio::spawn(async move {
            if let (Some(input), Some(mut stdin)) = (input, stdin) {
                let _ = stdin.write_all(input.as_bytes()).await;
            }
        });
        let waited = tokio::time::timeout(TIMEOUT, child.wait()).await;
        writer.abort();
        let Ok(status) = waited else {
            stdout.abort();
            stderr.abort();
            return Err(format!(
                "no answer within {} minutes",
                TIMEOUT.as_secs() / 60
            ));
        };
        let status = status.map_err(|e| e.to_string())?;
        group.exited();
        let drained = |task: tokio::task::JoinHandle<Vec<u8>>| async move {
            let abort = task.abort_handle();
            let out = tokio::time::timeout(DRAIN, task).await;
            abort.abort();
            out.ok().and_then(Result::ok).unwrap_or_default()
        };
        Ok(Output {
            status,
            stdout: drained(stdout).await,
            stderr: drained(stderr).await,
        })
    }
}

/// Reads `pipe` to its end, keeping the last [`OUTPUT_CAP`] bytes; reading on rather than
/// stopping keeps a chatty process from blocking on a full pipe.
async fn keep_end(pipe: Option<impl AsyncRead + Unpin>) -> Vec<u8> {
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut kept = Vec::new();
    let mut chunk = vec![0; 64 * 1024];
    while let Ok(n @ 1..) = pipe.read(&mut chunk).await {
        kept.extend_from_slice(&chunk[..n]);
        if kept.len() > OUTPUT_CAP {
            kept.drain(..kept.len() - OUTPUT_CAP);
        }
    }
    kept
}

/// A run's own directory for the files mothership hands Codex (its schema, its final message)
/// and its `TMPDIR`: under `root`, which every session's rules deny and only this run's profile
/// opens, named so nobody can guess it, and removed after the run.
pub(super) struct RunDir(pub(super) PathBuf);

impl RunDir {
    pub(super) fn create(root: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let dir = root.join(request::unique());
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The last `n` lines of a process's output.
pub(super) fn tail(bytes: &[u8], n: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// A run's process group (its leader's pid). Dropped while the leader runs (a timeout, the
/// turn ending), it kills the leader's whole tree, since Codex starts commands in groups of
/// their own, and then the group. Once the leader exited, only the group is left to kill.
// ponytail: a process that left both the tree and the group (setsid, a double fork) survives;
// a per-run launchd job or a user per run would catch it.
struct Group(Option<u32>);

impl Group {
    fn exited(&mut self) {
        if let Some(pgid) = self.0.take() {
            kill_group(pgid);
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            crate::agent::kill_tree(pid);
            kill_group(pid);
        }
    }
}

fn kill_group(pgid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stderr(Stdio::null())
        .status();
}
