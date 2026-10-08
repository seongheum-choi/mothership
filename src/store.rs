//! Durable state: Linear OAuth tokens and per-session bookkeeping, in `<home>/state.json`.

use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::{
    collections::HashMap,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
}

/// One conversation: a Linear agent session or a Zulip thread.
#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct SessionRec {
    /// Linear issue fields; empty for chat threads.
    pub issue_id: String,
    pub identifier: String,
    pub title: String,
    pub url: String,
    /// Name of the repository a Linear session works in, chosen once at its start.
    pub repo: Option<String>,
    /// Linear: the `<home>/modes` mode its labels picked at its start; `None` works as before.
    pub mode: Option<String>,
    /// Linear: the mode is not settled yet (labels conflicted, or a mode file did not parse),
    /// so no turn may run. Sessions from before modes have it false and keep the default.
    pub mode_pending: bool,
    #[serde(alias = "worktree")]
    pub workspace: Option<PathBuf>,
    pub branch: Option<String>,
    pub claude_session_id: Option<String>,
    /// Linear: the first prompt of a session still waiting to be told its repository.
    pub pending_prompt: Option<String>,
    /// Zulip: id of the newest topic message the agent has already seen.
    pub cursor: Option<u64>,
    /// Unix seconds of the newest prompt, so GitHub feedback on a branch that several
    /// sessions of one issue share goes to the latest of them.
    pub prompted_at: u64,
    /// Linear: the issue was closed and the session's worktree cleaned up, so GitHub feedback
    /// no longer starts turns. A new Linear prompt reopens it.
    pub closed: bool,
    /// Linear: pull request URLs already added to the session's external URLs.
    pub pull_requests: Vec<String>,
    /// `[model=…]` and `[effort=…]` from the session's messages; `None` follows the mode and
    /// the instance defaults.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Set while the model a message chose has not finished a turn: the `model` it replaced,
    /// which comes back if that turn fails.
    pub replaced_model: Option<Replaced>,
    /// The model and effort the latest turn started with.
    pub launched: Option<Launched>,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
pub struct Replaced {
    pub model: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
pub struct Launched {
    pub model: String,
    pub effort: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    #[serde(default)]
    pub linear: Tokens,
    #[serde(default)]
    pub sessions: HashMap<String, SessionRec>,
}

pub struct Store {
    path: PathBuf,
    state: Mutex<State>,
}

impl Store {
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    pub fn read<T>(&self, f: impl FnOnce(&State) -> T) -> T {
        f(&self.state.lock().expect("store lock poisoned"))
    }

    /// Applies `f` and persists the result. A failed write is logged, not returned: the
    /// in-memory state stays authoritative and the next update retries the write.
    pub fn update<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        let mut state = self.state.lock().expect("store lock poisoned");
        let out = f(&mut state);
        let written = serde_json::to_vec_pretty(&*state)
            .map_err(std::io::Error::from)
            .and_then(|bytes| write_private(&self.path, &bytes));
        if let Err(e) = written {
            tracing::error!("saving {}: {e}", self.path.display());
        }
        out
    }
}

/// Atomic 0600 write: readers see the old file or the new one, never a partial one.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", random_hex(4)));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    std::fs::rename(&tmp, path)
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("/dev/urandom is readable");
    buf.iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A session key as a single file or directory name.
pub fn file_name(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
