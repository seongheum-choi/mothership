mod linear;
mod session;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::Redirect,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;

pub type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct Config {
    pub home: PathBuf,
    pub bind: String,
    pub base_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub webhook_secret: String,
    pub repo: PathBuf,
    pub base_branch: String,
    pub worktrees_dir: PathBuf,
    pub claude_bin: String,
    pub model: String,
    pub fallback_model: String,
    /// Extra MCP config files passed to every Claude session next to the Linear one.
    pub mcp_configs: Vec<String>,
    /// Replaces the default "commit, push, open a PR" instructions.
    pub extra_prompt: Option<String>,
    pub cloudflare_token: Option<String>,
    pub cloudflared_bin: String,
    /// Seeds the token store on first start, e.g. tokens copied from ~/.cyrus/config.json.
    pub seed_tokens: Option<Tokens>,
}

impl Config {
    /// Process env wins over `<home>/.env`.
    fn load() -> Config {
        let user_home = PathBuf::from(std::env::var("HOME").expect("HOME"));
        let home = std::env::var("MOTHERSHIP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| user_home.join(".mothership"));
        let file: HashMap<String, String> = std::fs::read_to_string(home.join(".env"))
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
            .collect();
        let get = |k: &str| {
            std::env::var(k)
                .ok()
                .or_else(|| file.get(k).cloned())
                .filter(|v| !v.is_empty())
        };
        let req = |k: &str| {
            get(k).unwrap_or_else(|| {
                eprintln!(
                    "missing required setting {k} (env or {})",
                    home.join(".env").display()
                );
                std::process::exit(1)
            })
        };
        let extra_prompt = get("APPEND_SYSTEM_PROMPT_FILE").map(|p| {
            std::fs::read_to_string(&p).unwrap_or_else(|e| {
                eprintln!("APPEND_SYSTEM_PROMPT_FILE {p}: {e}");
                std::process::exit(1)
            })
        });
        let seed_tokens = get("LINEAR_ACCESS_TOKEN").map(|access_token| Tokens {
            access_token,
            refresh_token: get("LINEAR_REFRESH_TOKEN").unwrap_or_default(),
        });
        Config {
            bind: get("BIND").unwrap_or_else(|| "127.0.0.1:3456".into()),
            base_url: req("BASE_URL").trim_end_matches('/').to_string(),
            client_id: req("LINEAR_CLIENT_ID"),
            client_secret: req("LINEAR_CLIENT_SECRET"),
            webhook_secret: req("LINEAR_WEBHOOK_SECRET"),
            repo: PathBuf::from(req("REPO_PATH")),
            base_branch: get("BASE_BRANCH").unwrap_or_else(|| "main".into()),
            worktrees_dir: get("WORKTREES_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("worktrees")),
            claude_bin: get("CLAUDE_BIN").unwrap_or_else(|| "claude".into()),
            model: get("CLAUDE_MODEL").unwrap_or_else(|| "opus".into()),
            fallback_model: get("CLAUDE_FALLBACK_MODEL").unwrap_or_else(|| "sonnet".into()),
            mcp_configs: get("MCP_CONFIGS")
                .map(|s| s.split(',').map(|p| p.trim().to_string()).collect())
                .unwrap_or_default(),
            extra_prompt,
            cloudflare_token: get("CLOUDFLARE_TOKEN"),
            cloudflared_bin: get("CLOUDFLARED_BIN").unwrap_or_else(|| "cloudflared".into()),
            seed_tokens,
            home,
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SessionRec {
    pub issue_id: String,
    pub identifier: String,
    pub title: String,
    pub url: String,
    pub worktree: Option<PathBuf>,
    pub branch: Option<String>,
    pub claude_session_id: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Store {
    #[serde(default)]
    pub linear: Tokens,
    #[serde(default)]
    pub sessions: HashMap<String, SessionRec>,
}

pub struct App {
    pub cfg: Config,
    pub http: reqwest::Client,
    pub store: Mutex<Store>,
    /// Agent sessions with a running worker, keyed by Linear agent session id.
    pub live: Mutex<HashMap<String, mpsc::UnboundedSender<session::Msg>>>,
    pub refresh_lock: tokio::sync::Mutex<()>,
    oauth_state: Mutex<Option<String>>,
}

impl App {
    /// Applies `f` to the store and writes it to disk atomically (it holds OAuth tokens, so 0600).
    pub fn update<T>(&self, f: impl FnOnce(&mut Store) -> T) -> T {
        let mut store = self.store.lock().unwrap();
        let out = f(&mut store);
        let path = self.cfg.home.join("state.json");
        let tmp = path.with_extension("json.tmp");
        let write = || -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            serde_json::to_writer_pretty(&mut file, &*store)?;
            std::fs::rename(&tmp, &path)
        };
        if let Err(e) = write() {
            eprintln!("saving {}: {e}", path.display());
        }
        out
    }
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("/dev/urandom");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Same contract as Cyrus' CLOUDFLARE_TOKEN: a remotely-managed tunnel whose hostname
/// and origin (http://localhost:<port>) are configured in the Cloudflare dashboard.
async fn run_tunnel(bin: String, token: String) {
    loop {
        let status = tokio::process::Command::new(&bin)
            .args(["tunnel", "--no-autoupdate", "run"])
            .env("TUNNEL_TOKEN", &token) // env, not argv, so the token stays out of `ps`
            .kill_on_drop(true)
            .status()
            .await;
        eprintln!("cloudflared exited ({status:?}), restarting in 5s");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn linear_webhook(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let signature = headers
        .get("linear-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    if !linear::verify(&app.cfg.webhook_secret, &body, signature, now_ms) {
        return StatusCode::UNAUTHORIZED;
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    // Linear wants a 200 within 5 seconds; the work happens in the background.
    tokio::spawn(session::handle(app, payload));
    StatusCode::OK
}

async fn status(State(app): State<Arc<App>>) -> Json<Value> {
    let busy = !app.live.lock().unwrap().is_empty();
    Json(json!({ "status": if busy { "busy" } else { "idle" } }))
}

async fn authorize(State(app): State<Arc<App>>) -> Redirect {
    let state = random_hex(16);
    *app.oauth_state.lock().unwrap() = Some(state.clone());
    let redirect_uri = format!("{}/callback", app.cfg.base_url);
    let url = reqwest::Url::parse_with_params(
        "https://linear.app/oauth/authorize",
        [
            ("client_id", app.cfg.client_id.as_str()),
            ("redirect_uri", &redirect_uri),
            ("response_type", "code"),
            ("scope", "read,write,app:assignable,app:mentionable"),
            ("actor", "app"),
            ("state", &state),
        ],
    )
    .unwrap();
    Redirect::to(url.as_str())
}

async fn callback(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> (StatusCode, String) {
    let expected = app.oauth_state.lock().unwrap().take();
    if expected.is_none() || q.get("state") != expected.as_ref() {
        return (
            StatusCode::BAD_REQUEST,
            "state mismatch; start again at /oauth/authorize".into(),
        );
    }
    let Some(code) = q.get("code") else {
        return (StatusCode::BAD_REQUEST, "missing code".into());
    };
    match app.exchange_code(code).await {
        Ok(tokens) => {
            app.update(|s| s.linear = tokens);
            (
                StatusCode::OK,
                "Linear authorized. You can close this tab.".into(),
            )
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("token exchange failed: {e}"),
        ),
    }
}

#[tokio::main]
async fn main() {
    let cfg = Config::load();
    std::fs::create_dir_all(&cfg.home).expect("create MOTHERSHIP_HOME");
    let mut store: Store = std::fs::read(cfg.home.join("state.json"))
        .ok()
        .map(|b| serde_json::from_slice(&b).expect("state.json is corrupt"))
        .unwrap_or_default();
    if store.linear.access_token.is_empty()
        && let Some(seed) = &cfg.seed_tokens
    {
        store.linear = seed.clone();
    }
    let tunnel = cfg
        .cloudflare_token
        .clone()
        .map(|t| (cfg.cloudflared_bin.clone(), t));
    let app = Arc::new(App {
        cfg,
        http: reqwest::Client::new(),
        store: Mutex::new(store),
        live: Mutex::default(),
        refresh_lock: tokio::sync::Mutex::new(()),
        oauth_state: Mutex::default(),
    });
    app.update(|_| ()); // persist seeded tokens
    if app.store.lock().unwrap().linear.access_token.is_empty() {
        eprintln!(
            "no Linear token yet: open {}/oauth/authorize",
            app.cfg.base_url
        );
    }
    if let Some((bin, token)) = tunnel {
        tokio::spawn(run_tunnel(bin, token));
    }

    let router = Router::new()
        .route("/linear-webhook", post(linear_webhook))
        .route("/webhook", post(linear_webhook)) // URL Cyrus-era Linear apps point at
        .route("/status", get(status))
        .route("/oauth/authorize", get(authorize))
        .route("/callback", get(callback))
        .with_state(app.clone());
    let listener = tokio::net::TcpListener::bind(&app.cfg.bind)
        .await
        .unwrap_or_else(|e| panic!("bind {}: {e}", app.cfg.bind));
    eprintln!("mothership listening on {}", app.cfg.bind);
    axum::serve(listener, router).await.unwrap();
}
