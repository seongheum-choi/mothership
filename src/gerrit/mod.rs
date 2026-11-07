//! Gerrit review: agents push changes and answer review comments on Gerrit with the account in
//! `GERRIT_USERNAME`, through git and the `gerrit-reply` wrapper.

mod reply;

use crate::config::GerritConfig;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Runtime state of the Gerrit backend.
pub struct Gerrit {
    /// Holds `gerrit-reply`; put first on agents' `PATH`.
    pub bin_dir: PathBuf,
}

impl Gerrit {
    /// Installs `<home>/gerrit-reply/gerrit-reply`.
    pub fn new(home: &Path) -> Result<Self> {
        Ok(Self {
            bin_dir: reply::install(home)?,
        })
    }
}

/// Adds what an agent needs to push to and answer on Gerrit to its environment `env`: the URL,
/// the account's HTTP credentials, and a git credential helper that hands them to git for URLs
/// under `GERRIT_URL` only, in place of any helper the global or system config has there. `inherited` is the `GIT_CONFIG_COUNT` the agent would otherwise get.
pub fn agent_env(cfg: &GerritConfig, env: &mut Vec<(String, String)>, inherited: Option<String>) {
    // Configuration through the environment adds to entries `AGENT_ENV` or mothership's own
    // environment already numbered, instead of replacing them.
    let count = env
        .iter()
        .rev()
        .find(|(k, _)| k == "GIT_CONFIG_COUNT")
        .map(|(_, v)| v.clone())
        .or(inherited)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let key = format!("credential.{}.helper", cfg.url);
    env.extend([
        ("GERRIT_URL".into(), cfg.url.clone()),
        ("GERRIT_USERNAME".into(), cfg.username.clone()),
        ("GERRIT_HTTP_PASSWORD".into(), cfg.http_password.clone()),
        // An empty helper empties the list first: a global `store` or keychain helper would
        // otherwise be asked before ours and be handed the password to save after a push.
        (format!("GIT_CONFIG_KEY_{count}"), key.clone()),
        (format!("GIT_CONFIG_VALUE_{count}"), String::new()),
        (format!("GIT_CONFIG_KEY_{}", count + 1), key),
        (
            format!("GIT_CONFIG_VALUE_{}", count + 1),
            CREDENTIAL_HELPER.into(),
        ),
        ("GIT_CONFIG_COUNT".into(), (count + 2).to_string()),
    ]);
}

/// Answers git's `get` with the credentials from the environment, so they are never written to
/// a file in the agent's reach; `store` and `erase` do nothing.
const CREDENTIAL_HELPER: &str = "!f() { if [ \"$1\" = get ]; then printf 'username=%s\\npassword=%s\\n' \
     \"$GERRIT_USERNAME\" \"$GERRIT_HTTP_PASSWORD\"; fi; }; f";

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GerritConfig {
        GerritConfig {
            url: "https://gerrit.example.com".into(),
            username: "acme-bot".into(),
            http_password: "secret".into(),
        }
    }

    fn get<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn credential_helper_takes_the_next_free_git_config_slot() {
        let mut env = Vec::new();
        agent_env(&cfg(), &mut env, None);
        assert_eq!(get(&env, "GIT_CONFIG_COUNT"), Some("2"));
        for key in ["GIT_CONFIG_KEY_0", "GIT_CONFIG_KEY_1"] {
            assert_eq!(
                get(&env, key),
                Some("credential.https://gerrit.example.com.helper")
            );
        }
        assert_eq!(
            get(&env, "GIT_CONFIG_VALUE_0"),
            Some(""),
            "empties the list"
        );
        assert_eq!(get(&env, "GIT_CONFIG_VALUE_1"), Some(CREDENTIAL_HELPER));
        assert_eq!(get(&env, "GERRIT_HTTP_PASSWORD"), Some("secret"));

        let mut env = vec![("GIT_CONFIG_COUNT".to_string(), "2".to_string())];
        agent_env(&cfg(), &mut env, Some("5".into()));
        assert_eq!(get(&env, "GIT_CONFIG_COUNT"), Some("4"), "AGENT_ENV wins");
        assert!(get(&env, "GIT_CONFIG_KEY_3").is_some());

        let mut env = Vec::new();
        agent_env(&cfg(), &mut env, Some("1".into()));
        assert_eq!(get(&env, "GIT_CONFIG_COUNT"), Some("3"), "inherited");
        assert!(get(&env, "GIT_CONFIG_KEY_2").is_some());
    }

    #[test]
    fn git_gets_the_credentials_for_gerrit_urls_only() {
        // A global helper that answers every URL and logs each operation it is asked for, as
        // `store` or a keychain would be asked to save the password after a push.
        let dir = std::env::temp_dir().join(format!("mothership-gerrit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let log = dir.join("global-helper.log");
        let global = dir.join("gitconfig");
        let helper = format!(
            "!f() {{ echo \"$1\" >> '{}'; echo username=global; echo password=global-secret; }}; f",
            log.display()
        );
        std::fs::write(&global, format!("[credential]\n\thelper = \"{helper}\"\n"))
            .expect("writing the global config");
        let mut env = Vec::new();
        agent_env(&cfg(), &mut env, None);
        let git = |op: &str, input: &str| {
            let mut child = std::process::Command::new("git")
                .args(["credential", op])
                .envs(env.iter().cloned())
                .env("GIT_CONFIG_GLOBAL", &global)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_ASKPASS", "false")
                .env_remove("SSH_ASKPASS")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("git runs");
            let mut stdin = child.stdin.take().expect("piped stdin");
            std::io::Write::write_all(&mut stdin, input.as_bytes()).expect("git reads its input");
            drop(stdin);
            let out = child.wait_with_output().expect("git finishes");
            String::from_utf8(out.stdout).expect("utf-8")
        };
        let logged = || std::fs::read_to_string(&log).unwrap_or_default();

        let gerrit = "url=https://gerrit.example.com/a/team/app\n";
        let found = git("fill", &format!("{gerrit}\n"));
        assert!(
            found.contains("username=acme-bot\npassword=secret\n"),
            "{found}"
        );
        git(
            "approve",
            &format!("{gerrit}username=acme-bot\npassword=secret\n\n"),
        );
        assert_eq!(logged(), "", "the global helper never hears of Gerrit");

        let found = git("fill", "url=https://github.com/o/r\n\n");
        assert!(found.contains("password=global-secret"), "{found}");
        assert!(!found.contains("password=secret"));
        assert_eq!(logged(), "get\n", "other hosts keep the global helper");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
