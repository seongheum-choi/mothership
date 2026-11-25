//! The container runtime `sandbox` runs start their containers through: its CLI and the
//! environment it gets, and, with `CONTAINER_START` and `CONTAINER_STOP`, starting it when a run
//! finds it down and stopping it once it has started it and no run has needed it for
//! `CONTAINER_IDLE`.

use std::{
    process::Stdio,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::process::Command;

/// Of mothership's environment, what the runtime CLI gets to find its daemon. The container
/// gets none of it.
pub(super) const RUNTIME_ENV: [&str; 10] = [
    "PATH",
    "HOME",
    // Where rootless nerdctl finds its containerd and rootlesskit.
    "XDG_RUNTIME_DIR",
    "CONTAINERD_ADDRESS",
    "CONTAINERD_NAMESPACE",
    "CONTAINERD_SNAPSHOTTER",
    "NERDCTL_TOML",
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_CONFIG",
];
/// How long `CONTAINER_START` or `CONTAINER_STOP` may take.
const LIMIT: Duration = Duration::from_mins(3);
/// How long `<cli> info` may take to say the runtime is up.
const CHECK: Duration = Duration::from_secs(30);

/// `CONTAINER_RUNTIME`, shared by every session, with what starts and stops it.
pub struct Runtime {
    /// The CLI: nerdctl, or another Docker-compatible one.
    pub cli: String,
    /// `CONTAINER_START`, run with `/bin/sh -c` in mothership's own environment.
    start: Option<String>,
    /// `CONTAINER_STOP`, the same.
    stop: Option<String>,
    /// `CONTAINER_IDLE`: how long after the last run a runtime mothership started is stopped.
    idle: Duration,
    /// How long either command may take: [`LIMIT`], shorter in tests.
    limit: Duration,
    state: Mutex<State>,
    /// Held while checking, starting or stopping, so two runs never start it twice and a run
    /// never finds it half stopped.
    gate: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct State {
    /// Runs holding a [`Lease`].
    active: usize,
    /// Leases ever taken, so an idle stop can tell whether a run came while it waited.
    taken: u64,
    /// Whether mothership started the runtime and has not stopped it since: it never stops one
    /// the operator started.
    started: bool,
}

/// A run's hold on the runtime: while any is held, it is not stopped.
pub struct Lease(Arc<Runtime>);

impl Runtime {
    pub fn new(cli: String, start: Option<String>, stop: Option<String>, idle: Duration) -> Self {
        Self {
            cli,
            start,
            stop,
            idle,
            limit: LIMIT,
            state: Mutex::default(),
            gate: tokio::sync::Mutex::new(()),
        }
    }

    /// Holds the runtime for a run, first starting it with `CONTAINER_START` when there is one
    /// and the runtime does not answer.
    pub async fn acquire(self: &Arc<Self>) -> Result<Lease, String> {
        {
            let mut state = self.state();
            state.active += 1;
            state.taken += 1;
        }
        let lease = Lease(Arc::clone(self));
        let Some(start) = &self.start else {
            return Ok(lease);
        };
        let _gate = self.gate.lock().await;
        if self.responds().await {
            return Ok(lease);
        }
        tracing::info!("the container runtime does not answer; running `{start}`");
        run(start, self.limit)
            .await
            .map_err(|e| format!("the container runtime was down and `{start}` {e}"))?;
        self.state().started = true;
        Ok(lease)
    }

    /// The CLI with only [`RUNTIME_ENV`], its output read, killed when dropped.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.cli);
        command
            .env_clear()
            .envs(
                RUNTIME_ENV
                    .iter()
                    .filter_map(|k| Some((k, std::env::var_os(k)?))),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    async fn responds(&self) -> bool {
        let info = tokio::time::timeout(CHECK, self.command().arg("info").output()).await;
        matches!(info, Ok(Ok(out)) if out.status.success())
    }

    /// After [`Self::idle`], stops the runtime when mothership started it and no run took a
    /// lease since the one that ended as lease `taken`.
    async fn stop_when_idle(self: Arc<Self>, taken: u64) {
        tokio::time::sleep(self.idle).await;
        let Some(stop) = &self.stop else { return };
        let _gate = self.gate.lock().await;
        {
            let state = self.state();
            if state.active > 0 || state.taken != taken || !state.started {
                return;
            }
        }
        match run(stop, self.limit).await {
            Ok(()) => {
                self.state().started = false;
                tracing::info!("stopped the idle container runtime with `{stop}`");
            }
            Err(e) => tracing::warn!("cannot stop the idle container runtime: `{stop}` {e}"),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("the runtime's state is only held for a few assignments")
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.active -= 1;
        if state.active == 0 && state.started && self.0.stop.is_some() {
            // A lease is only dropped by a run, inside the runtime; at shutdown there is none.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(Arc::clone(&self.0).stop_when_idle(state.taken));
            }
        }
    }
}

/// Runs operator-set `command` with `/bin/sh -c` in mothership's environment, its output in
/// mothership's log, killed after `limit`.
async fn run(command: &str, limit: Duration) -> Result<(), String> {
    let mut child = Command::new("/bin/sh")
        .args(["-c", command])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start: {e}"))?;
    match tokio::time::timeout(limit, child.wait()).await {
        Err(_) => Err(format!("did not finish within {} seconds", limit.as_secs())),
        Ok(Err(e)) => Err(format!("could not be waited for: {e}")),
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(format!("failed ({status})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A directory where the fake runtime is up while `up` exists, its start and stop counted
    /// in `starts` and `stops`.
    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mothership-rt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let cli = d.join("cli");
        std::fs::write(&cli, format!("#!/bin/sh\n[ -f '{}/up' ]\n", d.display())).unwrap();
        let mut perms = std::fs::metadata(&cli).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&cli, perms).unwrap();
        d
    }

    fn runtime(d: &Path, start: &str) -> Arc<Runtime> {
        let d = d.display();
        Arc::new(Runtime {
            limit: Duration::from_millis(500),
            ..Runtime::new(
                format!("{d}/cli"),
                Some(start.replace("DIR", &d.to_string())),
                Some(format!("echo >> '{d}/stops'; rm '{d}/up'")),
                Duration::from_millis(200),
            )
        })
    }

    const START: &str = "sleep 0.2; echo >> 'DIR/starts'; touch 'DIR/up'";

    fn count(d: &Path, file: &str) -> usize {
        std::fs::read_to_string(d.join(file)).map_or(0, |s| s.lines().count())
    }

    async fn idle() {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    #[tokio::test]
    async fn starts_a_down_runtime_once_for_concurrent_runs() {
        let d = dir("once");
        let rt = runtime(&d, START);
        let (a, b) = tokio::join!(rt.acquire(), rt.acquire());
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(count(&d, "starts"), 1);
        assert!(d.join("up").exists());
    }

    #[tokio::test]
    async fn stops_only_what_it_started_once_idle() {
        let d = dir("idle");
        let rt = runtime(&d, START);
        drop(rt.acquire().await.unwrap());
        // A run during the idle wait keeps it up, and the wait starts again when it ends.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let lease = rt.acquire().await.unwrap();
        idle().await;
        assert_eq!((count(&d, "starts"), count(&d, "stops")), (1, 0));
        drop(lease);
        idle().await;
        assert_eq!(count(&d, "stops"), 1);
        assert!(!d.join("up").exists());

        // Up before mothership needed it: the operator's, left running.
        std::fs::write(d.join("up"), "").unwrap();
        let rt = runtime(&d, START);
        drop(rt.acquire().await.unwrap());
        idle().await;
        assert_eq!((count(&d, "starts"), count(&d, "stops")), (1, 1));
    }

    #[tokio::test]
    async fn answers_an_error_when_the_start_fails_or_hangs() {
        let d = dir("fail");
        let rt = runtime(&d, "exit 3");
        assert!(rt.acquire().await.err().unwrap().contains("failed"));
        // A failed run holds nothing: nothing waits on it to stop the runtime.
        assert_eq!(rt.state().active, 0);
        let hung = runtime(&d, "sleep 5").acquire().await;
        assert!(hung.err().unwrap().contains("did not finish"));
    }

    #[tokio::test]
    async fn leaves_the_runtime_alone_without_a_start_command() {
        let d = dir("none");
        let rt = Arc::new(Runtime::new(
            format!("{}/cli", d.display()),
            None,
            None,
            Duration::ZERO,
        ));
        assert!(rt.acquire().await.is_ok());
        assert_eq!(count(&d, "starts"), 0);
    }
}
