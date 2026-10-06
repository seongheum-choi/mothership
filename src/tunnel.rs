//! Public ingress for webhooks. Any tunnel is a supervised child process; adding one (ngrok,
//! tailscale funnel, ...) means adding a variant and its command.
//!
//! Running no tunnel at all is fine too: anything that forwards to `BIND` works.

use std::time::Duration;
use tokio::process::Command;

pub enum Tunnel {
    /// A remotely-managed Cloudflare tunnel. Its hostname → `http://localhost:<port>` route
    /// is configured in the Cloudflare dashboard.
    Cloudflare { bin: String, token: String },
}

impl Tunnel {
    fn command(&self) -> Command {
        match self {
            Self::Cloudflare { bin, token } => {
                let mut cmd = Command::new(bin);
                cmd.args(["tunnel", "--no-autoupdate", "run"])
                    .env("TUNNEL_TOKEN", token); // env, not argv, so it stays out of `ps`
                cmd
            }
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Cloudflare { .. } => "cloudflared",
        }
    }

    /// Runs the tunnel for the life of the process, restarting it 5 seconds after it exits.
    pub async fn supervise(self) {
        loop {
            let status = self.command().kill_on_drop(true).status().await;
            tracing::warn!("{} exited ({status:?}), restarting in 5s", self.name());
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}
