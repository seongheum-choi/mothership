//! A command the repository names in `repos.json` `commands`, for work the agent's sandbox
//! refuses: Claude Code's denies writes to any `.idea` directory, which a dependency that ships
//! one turns into a failed `pnpm install`. It runs under the session's own settings, without
//! Codex's login readable.

use super::Host;
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};

/// What the session's system prompt says about `commands`, when the repository has any.
pub fn prompt(commands: &BTreeMap<String, String>) -> Option<String> {
    if commands.is_empty() {
        return None;
    }
    let list: Vec<String> = commands
        .iter()
        .map(|(name, command)| format!("- `{name}`: `{command}`"))
        .collect();
    Some(format!(
        "Your Bash sandbox refuses some of this repository's setup, so mothership runs these \
         commands for you in your workspace, under the same file rules but for `.idea` directories. \
         To run one, write `{{\"kind\": \"command\", \"name\": \"<name>\"}}` to a file in \
         `$MOTHERSHIP_RUNS` under a temporary name, rename it to `<id>.json`, and wait for \
         `<id>.out.json` there: `{{\"result\": <its last lines>}}`, or `{{\"error\": …}}` when it \
         failed; its output, up to its last mebibyte, is in `<id>.log` there. Run them this way instead of in Bash, and when the repository's own \
         instructions ask for them.\n{}\n\n",
        list.join("\n")
    ))
}

impl Host {
    /// Runs `name` and writes its output, stderr interleaved, to `log`; answers with the
    /// output's last lines and where the rest is.
    pub(super) async fn command(
        &self,
        dir: &Path,
        name: &str,
        log: &Path,
    ) -> Result<Value, String> {
        let command = self
            .commands
            .get(name)
            .ok_or_else(|| format!("no command `{name}` in this repository's `commands`"))?;
        let args = ["-c".to_string(), format!("exec 2>&1\n{command}")];
        let out = self
            .confined(
                self.profile(&self.settings, dir, Vec::new()),
                dir,
                "/bin/sh",
                &args,
                None,
            )
            .await?;
        let output = super::process::tail(&out.stdout, 20);
        let output = match super::request::write_new(log, &out.stdout) {
            Ok(()) => format!("{output}\n\nWhole output: {}", log.display()),
            Err(e) => format!("{output}\n\nWhole output not kept: {e}"),
        };
        if out.status.success() {
            Ok(Value::String(output))
        } else {
            Err(format!("`{name}` exited ({}):\n{output}", out.status))
        }
    }
}
