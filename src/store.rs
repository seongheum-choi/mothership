//! Durable state: Linear OAuth tokens and per-session bookkeeping, in `<home>/state.json`.

use crate::agent::Runner;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::{
    collections::{BTreeMap, HashMap},
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
    /// The session the next turn on each runner continues: a Claude session id or a Codex
    /// thread id. Kept per runner, so a session back on Claude Code after its usage limit
    /// resumes where Claude Code left off.
    pub agent_sessions: BTreeMap<Runner, String>,
    /// From before per-runner ids: the one session kept, and the runner it belongs to (`None`,
    /// from before runners, is Claude Code). Moved into `agent_sessions` on load.
    #[serde(alias = "claude_session_id", skip_serializing)]
    pub agent_session_id: Option<String>,
    #[serde(skip_serializing)]
    pub agent_runner: Option<Runner>,
    /// Linear: the first prompt and every reply since, kept while the session waits for its
    /// repository, mode or a corrected directive; run together once it can start.
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

impl SessionRec {
    /// The session a turn on `runner` continues. One runner cannot resume another's, so a
    /// session on a runner it has not used yet starts afresh there.
    pub fn resume(&self, runner: Runner) -> Option<String> {
        self.agent_sessions.get(&runner).cloned()
    }

    /// Records the session a turn on `runner` started, for the next turn on it to resume.
    pub fn set_resume(&mut self, runner: Runner, id: String) {
        self.agent_sessions.insert(runner, id);
    }

    /// Moves the single session id of a record from before per-runner ids into place.
    fn upgrade(&mut self) {
        if let Some(id) = self.agent_session_id.take() {
            let runner = self.agent_runner.take().unwrap_or_default();
            self.agent_sessions.entry(runner).or_insert(id);
        }
    }
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
    /// When Claude Code's usage limit resets (Unix seconds), as the turn that hit it said. Until
    /// then turns run on Codex; the whole instance shares the one account.
    #[serde(default)]
    pub claude_resets_at: Option<u64>,
}

pub struct Store {
    path: PathBuf,
    state: Mutex<State>,
}

impl Store {
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        let mut state: State = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.into()),
        };
        state.sessions.values_mut().for_each(SessionRec::upgrade);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn load(json: &str) -> SessionRec {
        let mut rec: SessionRec = serde_json::from_str(json).unwrap();
        rec.upgrade();
        rec
    }

    #[test]
    fn keeps_a_session_per_runner() {
        let mut rec = load(r#"{"claude_session_id":"c1"}"#);
        assert_eq!(rec.resume(Runner::Claude).as_deref(), Some("c1"));
        assert_eq!(rec.resume(Runner::Codex), None);
        rec.set_resume(Runner::Codex, "t1".into());
        assert_eq!(rec.resume(Runner::Codex).as_deref(), Some("t1"));
        assert_eq!(rec.resume(Runner::Claude).as_deref(), Some("c1"));
        let saved = serde_json::to_value(&rec).unwrap();
        assert_eq!(
            saved["agent_sessions"],
            serde_json::json!({"claude":"c1","codex":"t1"})
        );
        assert!(saved.get("agent_session_id").is_none(), "{saved}");
        assert_eq!(load(&saved.to_string()).agent_sessions, rec.agent_sessions);
    }

    #[test]
    fn upgrades_the_single_id_of_its_runner() {
        let rec = load(r#"{"agent_session_id":"t1","agent_runner":"codex"}"#);
        assert_eq!(rec.resume(Runner::Codex).as_deref(), Some("t1"));
        assert_eq!(rec.resume(Runner::Claude), None);
        assert!(load("{}").agent_sessions.is_empty());
    }
}
