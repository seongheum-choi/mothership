//! The control socket `<home>/cli.sock`, through which `mothership session` asks the running
//! instance. It is a Unix socket only its user may open, so nothing it answers or does is
//! reachable through the tunnel, which carries the HTTP port.

use crate::app::App;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};

/// The most a request may be: a reply body is a comment, far below this.
const MAX_REQUEST: u64 = 1 << 20;

/// What the CLI asks; it sends one as JSON, closes its side and reads the [`Answer`].
#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    Status {
        issue: String,
    },
    Reply {
        issue: String,
        body: String,
        dry_run: bool,
    },
}

/// The text to print, or why the request was refused.
#[derive(Serialize, Deserialize)]
pub struct Answer {
    pub ok: bool,
    pub text: String,
}

pub fn socket(home: &Path) -> PathBuf {
    home.join("cli.sock")
}

/// Listens on the control socket. Refuses to take it from another instance that still
/// answers there; a socket file left by one that has exited is replaced.
pub fn listen(app: &Arc<App>) -> Result<()> {
    let path = socket(&app.cfg.home);
    if std::os::unix::net::UnixStream::connect(&path).is_ok() {
        bail!("another mothership answers on {}", path.display());
    }
    crate::store::remove_if_present(&path)
        .with_context(|| format!("removing {}", path.display()))?;
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("making {} private", path.display()))?;
    tokio::spawn(accept(app.clone(), listener));
    Ok(())
}

/// Removes the socket file at shutdown, so the CLI says nothing runs instead of hanging.
pub fn remove(home: &Path) {
    let _ = std::fs::remove_file(socket(home));
}

async fn accept(app: Arc<App>, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(answer(app.clone(), stream));
            }
            Err(e) => {
                tracing::warn!("control socket: {e:#}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

async fn answer(app: Arc<App>, stream: UnixStream) {
    let (read, mut write) = stream.into_split();
    let mut buf = Vec::new();
    let answer = match read.take(MAX_REQUEST).read_to_end(&mut buf).await {
        Ok(_) => match serde_json::from_slice::<Request>(&buf) {
            Ok(request) => handle(&app, request).await,
            Err(e) => Err(anyhow!("not a request: {e}")),
        },
        Err(e) => Err(anyhow!("reading the request: {e}")),
    };
    let answer = match answer {
        Ok(text) => Answer { ok: true, text },
        Err(e) => {
            tracing::warn!("control socket: refused: {e:#}");
            Answer {
                ok: false,
                text: format!("{e:#}"),
            }
        }
    };
    let bytes = serde_json::to_vec(&answer).expect("an answer serializes");
    if let Err(e) = write.write_all(&bytes).await {
        tracing::warn!("control socket: answering: {e:#}");
    }
}

async fn handle(app: &Arc<App>, request: Request) -> Result<String> {
    app.refresh();
    let linear = &app.linear.surface;
    match request {
        Request::Status { issue } => linear.session_status(app, &issue).await,
        Request::Reply {
            issue,
            body,
            dry_run,
        } => linear.session_reply(app, &issue, &body, dry_run).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_name_their_command() {
        let reply = Request::Reply {
            issue: "SH-1".into(),
            body: "go on".into(),
            dry_run: true,
        };
        let json = serde_json::to_value(&reply).unwrap();
        assert_eq!(json["command"], "reply");
        assert_eq!(serde_json::from_value::<Request>(json).unwrap(), reply);
        let unknown = serde_json::json!({ "command": "delete", "issue": "SH-1" });
        assert!(serde_json::from_value::<Request>(unknown).is_err());
    }
}
