//! One mothership process under test: a fresh `HOME` with git repositories in it, a free
//! port, the mocks, and the fakes on `PATH`. Its helpers sign and send webhooks and wait for
//! what mothership does in response.

use crate::mock::{self, Mock};
use anyhow::{Context, Result, bail};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::process::{Child, Command};

pub const LINEAR_SECRET: &str = "linear-secret";
pub const GITHUB_SECRET: &str = "github-secret";
/// The agent's GitHub account; feedback must mention it.
pub const GITHUB_LOGIN: &str = "impala";
pub const ZULIP_TOKEN: &str = "zulip-token";
pub const ZULIP_BOT: &str = "bot@zulip.test";

/// How long `eventually` waits; everything here takes well under a second.
const PATIENCE: Duration = Duration::from_secs(15);

/// The directories a scenario's instances used: removed when it passes, kept and reported
/// when it fails.
#[derive(Clone, Default)]
pub struct Ctx {
    dirs: Arc<Mutex<Vec<PathBuf>>>,
}

impl Ctx {
    fn dirs(&self) -> Vec<PathBuf> {
        self.dirs.lock().expect("ctx lock poisoned").clone()
    }

    pub fn clean_up(&self) {
        for dir in self.dirs() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    pub fn report(&self) {
        for dir in self.dirs() {
            let log = std::fs::read_to_string(dir.join("mothership.log")).unwrap_or_default();
            let lines: Vec<&str> = log.lines().collect();
            let tail = &lines[lines.len().saturating_sub(40)..];
            println!("  kept {}; mothership.log ends:", dir.display());
            for line in tail {
                println!("    {line}");
            }
        }
    }
}

/// What the instance is configured with.
pub struct Setup {
    /// Repositories by name, with the GitHub `owner/name` their `origin` points at. A single
    /// one is `REPO_PATH`, several go into `repos.json`.
    pub repos: Vec<(&'static str, &'static str)>,
    /// Directories by name that are not git repositories; they go into `repos.json` with
    /// `"git": false`.
    pub folders: Vec<&'static str>,
    pub zulip: bool,
    pub github: bool,
    /// The seed `LINEAR_ACCESS_TOKEN`.
    pub token: &'static str,
    pub env: Vec<(&'static str, String)>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            repos: vec![("app", "o/r")],
            folders: Vec::new(),
            zulip: false,
            github: false,
            token: "token",
            env: Vec::new(),
        }
    }
}

pub struct Harness {
    pub dir: PathBuf,
    pub home: PathBuf,
    pub mock: Mock,
    child: Child,
    url: String,
    http: reqwest::Client,
}

impl Harness {
    pub async fn start(ctx: &Ctx, setup: Setup) -> Result<Self> {
        let dir = new_dir(ctx)?;
        let bin = dir.join("bin");
        let home = dir.join("home");
        let state = home.join(".mothership");
        std::fs::create_dir_all(&state)?;

        let mut env: Vec<(&str, String)> = Vec::new();
        let mut repos = Vec::new();
        for (name, origin) in &setup.repos {
            let path = home.join("src").join(name);
            create_repo(&path, origin)?;
            repos.push(json!({ "name": name, "path": path }));
        }
        for name in &setup.folders {
            let path = home.join("src").join(name);
            std::fs::create_dir_all(&path)?;
            repos.push(json!({ "name": name, "path": path, "git": false }));
        }
        if let [repo] = repos.as_slice()
            && setup.folders.is_empty()
        {
            env.push((
                "REPO_PATH",
                repo["path"].as_str().unwrap_or_default().into(),
            ));
        } else {
            std::fs::write(state.join("repos.json"), Value::Array(repos).to_string())?;
        }

        let mock = Mock::start().await?;
        let path = std::env::var("PATH").unwrap_or_default();
        env.extend([
            ("HOME", home.display().to_string()),
            ("MOTHERSHIP_HOME", state.display().to_string()),
            ("PATH", format!("{}:{path}", bin.display())),
            ("LINEAR_API_URL", mock.url.clone()),
            ("LINEAR_CLIENT_ID", "client".into()),
            ("LINEAR_CLIENT_SECRET", "client-secret".into()),
            ("LINEAR_WEBHOOK_SECRET", LINEAR_SECRET.into()),
            ("LINEAR_WORKSPACE", mock::ORG_URL_KEY.into()),
            ("LINEAR_ACCESS_TOKEN", setup.token.into()),
            ("LINEAR_REFRESH_TOKEN", "refresh".into()),
            ("CLAUDE_BIN", bin.join("claude").display().to_string()),
            ("FAKE_LOG", dir.join("fake.jsonl").display().to_string()),
            // `origin` points at github.com; fetching it fails at once instead of going online.
            ("GIT_ALLOW_PROTOCOL", "file".into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_TERMINAL_PROMPT", "0".into()),
        ]);
        if setup.zulip {
            env.extend([
                ("ZULIP_SITE", mock.url.clone()),
                ("ZULIP_BOT_EMAIL", ZULIP_BOT.into()),
                ("ZULIP_API_KEY", "zulip-key".into()),
                ("ZULIP_WEBHOOK_TOKEN", ZULIP_TOKEN.into()),
            ]);
        }
        if setup.github {
            env.push(("GITHUB_WEBHOOK_SECRET", GITHUB_SECRET.into()));
            env.push(("GITHUB_MENTION_LOGIN", GITHUB_LOGIN.into()));
        }
        env.extend(setup.env);

        let http = reqwest::Client::new();
        let log = dir.join("mothership.log");
        let mut attempt = 0;
        loop {
            // A free port can be taken by another scenario's mock before mothership binds it.
            attempt += 1;
            let port = free_port()?;
            let url = format!("http://127.0.0.1:{port}");
            let stdout = std::fs::File::create(&log)?;
            let mut child = Command::new(env!("CARGO_BIN_EXE_mothership"))
                .env_clear()
                .envs(env.iter().cloned())
                .env("BIND", format!("127.0.0.1:{port}"))
                .env("BASE_URL", &url)
                .current_dir(&dir)
                .stdin(Stdio::null())
                .stderr(stdout.try_clone()?)
                .stdout(stdout)
                .kill_on_drop(true)
                .spawn()
                .context("starting mothership")?;
            match wait_ready(&http, &url, &mut child).await {
                Ok(()) => {
                    return Ok(Self {
                        dir,
                        home,
                        mock,
                        child,
                        url,
                        http,
                    });
                }
                Err(_)
                    if attempt < 5
                        && std::fs::read_to_string(&log)
                            .unwrap_or_default()
                            .contains("Address already in use") => {}
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn post(&self, path: &str, headers: &[(&str, &str)], body: Vec<u8>) -> Result<u16> {
        let mut req = self
            .http
            .post(format!("{}{path}", self.url))
            .header("content-type", "application/json")
            .body(body);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        Ok(req.send().await?.status().as_u16())
    }

    /// Sends a Linear webhook stamped now and signed with the webhook secret.
    pub async fn linear(&self, mut payload: Value) -> Result<u16> {
        payload["webhookTimestamp"] = json!(now_ms());
        let body = payload.to_string().into_bytes();
        let signature = sign(LINEAR_SECRET, &body);
        self.post("/linear-webhook", &[("linear-signature", &signature)], body)
            .await
    }

    pub async fn github(&self, event: &str, delivery: &str, payload: &Value) -> Result<u16> {
        let body = payload.to_string().into_bytes();
        let signature = format!("sha256={}", sign(GITHUB_SECRET, &body));
        let headers = [
            ("x-github-event", event),
            ("x-github-delivery", delivery),
            ("x-hub-signature-256", signature.as_str()),
        ];
        self.post("/github-webhook", &headers, body).await
    }

    pub async fn zulip(&self, payload: &Value) -> Result<u16> {
        self.post("/zulip-webhook", &[], payload.to_string().into_bytes())
            .await
    }

    /// Activity contents posted to the Linear session `sid`, oldest first.
    pub fn activities(&self, sid: &str) -> Vec<Value> {
        self.mock.read(|r| {
            r.activities
                .iter()
                .filter(|a| a["agentSessionId"] == sid)
                .map(|a| a["content"].clone())
                .collect()
        })
    }

    /// Waits for the session's `n`th (from 1) activity of `kind` and returns its content.
    pub async fn activity(&self, sid: &str, kind: &str, n: usize) -> Result<Value> {
        eventually(&format!("{kind} activity #{n} on {sid}"), || {
            self.activities(sid)
                .into_iter()
                .filter(|a| a["type"] == kind)
                .nth(n - 1)
        })
        .await
        .with_context(|| format!("activities so far: {:#?}", self.activities(sid)))
    }

    /// Everything the fakes were asked, oldest first, filtered by `tool` (`claude` or `gh`).
    pub fn calls(&self, tool: &str) -> Vec<Value> {
        std::fs::read_to_string(self.dir.join("fake.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|c| c["tool"] == tool)
            .collect()
    }

    /// The prompts the fake agent received that contain `needle`.
    pub fn prompts_with(&self, needle: &str) -> Vec<Value> {
        self.calls("claude")
            .into_iter()
            .filter(|c| c["prompt"].as_str().is_some_and(|p| p.contains(needle)))
            .collect()
    }

    /// Waits until the fake agent has received a prompt containing `needle`, and returns it.
    pub async fn prompt_with(&self, needle: &str) -> Result<Value> {
        eventually(&format!("a prompt containing {needle:?}"), || {
            self.prompts_with(needle).into_iter().next()
        })
        .await
    }

    /// The session records in `state.json`, by session id.
    pub fn sessions(&self) -> Result<Value> {
        let state = std::fs::read(self.home.join(".mothership/state.json"))?;
        Ok(serde_json::from_slice::<Value>(&state)?["sessions"].take())
    }

    /// Waits until mothership has logged a line containing `needle`: the only sign of work it
    /// decided not to do.
    pub async fn logged(&self, needle: &str) -> Result<()> {
        let log = self.dir.join("mothership.log");
        eventually(&format!("a log line containing {needle:?}"), || {
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .contains(needle)
                .then_some(())
        })
        .await
    }

    /// Sends SIGTERM and waits for mothership to exit.
    pub async fn terminate(&mut self) -> Result<ExitStatus> {
        let pid = self.child.id().context("mothership already exited")?;
        signal("-TERM", pid)?;
        tokio::time::timeout(PATIENCE, self.child.wait())
            .await
            .context("mothership did not exit after SIGTERM")?
            .context("waiting for mothership")
    }
}

/// A fresh directory for one instance, recorded in `ctx`, with the fakes in its `bin`.
fn new_dir(ctx: &Ctx) -> Result<PathBuf> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "mothership-it-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin"))?;
    // macOS reaches the temp directory through a symlink; git reports resolved paths.
    let dir = dir.canonicalize()?;
    ctx.dirs
        .lock()
        .expect("ctx lock poisoned")
        .push(dir.clone());
    let me = std::env::current_exe().context("locating the test executable")?;
    for name in ["claude", "gh"] {
        std::os::unix::fs::symlink(&me, dir.join("bin").join(name))?;
    }
    Ok(dir)
}

/// Waits until mothership answers `/status`, failing early if it exits.
async fn wait_ready(http: &reqwest::Client, url: &str, child: &mut Child) -> Result<()> {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("mothership exited during startup ({status})");
        }
        let status = http.get(format!("{url}/status")).send().await;
        if status.is_ok_and(|r| r.status().is_success()) {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("mothership did not answer /status within {PATIENCE:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Polls `probe` until it returns something, for up to [`PATIENCE`].
pub async fn eventually<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> Result<T> {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(found) = probe() {
            return Ok(found);
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits for process `pid` to be gone.
pub async fn exited(pid: u64) -> Result<()> {
    let pid = u32::try_from(pid).context("pid out of range")?;
    eventually(&format!("process {pid} to exit"), || {
        signal("-0", pid).is_err().then_some(())
    })
    .await
}

fn signal(which: &str, pid: u32) -> Result<()> {
    let status = std::process::Command::new("kill")
        .args([which, &pid.to_string()])
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        bail!("kill {which} {pid} failed");
    }
    Ok(())
}

/// Runs git with no user or system configuration, so local signing or hooks stay out.
pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A clone with one commit on `main`, `origin` on GitHub, and `origin/main` already fetched.
fn create_repo(path: &Path, origin: &str) -> Result<()> {
    std::fs::create_dir_all(path)?;
    git(path, &["init", "-q", "-b", "main"])?;
    git(path, &["commit", "-q", "--allow-empty", "-m", "initial"])?;
    let url = format!("https://github.com/{origin}.git");
    git(path, &["remote", "add", "origin", &url])?;
    git(path, &["update-ref", "refs/remotes/origin/main", "HEAD"])?;
    Ok(())
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// hex(HMAC-SHA256(secret, body)), as Linear and GitHub sign webhooks.
pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key length");
    mac.update(body);
    mac.finalize()
        .into_bytes()
        .iter()
        .fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}
