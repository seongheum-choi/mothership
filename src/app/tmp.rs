//! Each session's temporary directory (`TMPDIR` and `CLAUDE_CODE_TMPDIR`): where it is, how long
//! its path may be, creating it, and the rules that keep other sessions out of it.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs::{DirBuilder, Permissions},
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// A Unix socket path's limit, its NUL included: `sun_path` in `sockaddr_un`.
const SUN_PATH: usize = if cfg!(target_os = "macos") { 104 } else { 108 };
/// What Claude Code's sandbox adds to `TMPDIR` for its bridge sockets: `/srt-mux-<pid>-<n>.sock`.
const SOCKET_NAME: usize = 30;
/// The longest `<CLAUDE_CODE_TMPDIR>/claude-<uid>` Claude Code on macOS gives Bash as `TMPDIR`;
/// past it (`GBr` in 2.1.321) Bash gets the shared `/tmp/claude-<uid>` instead.
const CLAUDE_CHILD_TMPDIR: usize = 44;

/// Where the sessions' temporary directories are, and the user who must own them.
#[derive(Clone, Debug)]
pub struct TmpRoot {
    /// `/tmp/ms-<uid>` when `short`, else `<home>/session-tmp`.
    pub dir: PathBuf,
    uid: u32,
    /// Outside `MOTHERSHIP_HOME`, so the home directory's read rules do not cover it.
    short: bool,
}

impl TmpRoot {
    /// The instance's root: `/tmp/ms-<uid>` on macOS, where Claude Code needs the short path
    /// (see [`CLAUDE_CHILD_TMPDIR`]), unless `MOTHERSHIP_ALLOW_LONG_TMPDIR` says its `claude`
    /// makes no sockets (the integration tests', whose sandbox cannot write `/tmp`).
    pub fn for_instance(state: &Path, uid: u32) -> Self {
        let long = std::env::var_os("MOTHERSHIP_ALLOW_LONG_TMPDIR").is_some();
        Self::new(state, uid, cfg!(target_os = "macos") && !long)
    }

    fn new(state: &Path, uid: u32, short: bool) -> Self {
        let dir = if short {
            PathBuf::from(format!("/tmp/ms-{uid}"))
        } else {
            state.join("session-tmp")
        };
        Self { dir, uid, short }
    }

    /// Session `key`'s directory: 16 hex digits of the key's SHA-256, the same length however
    /// long the key is (a Zulip topic's is the topic), since sockets are made in it.
    pub fn session(&self, key: &str) -> PathBuf {
        let digest = Sha256::digest(key.as_bytes());
        let head: [u8; 8] = digest[..8].try_into().expect("SHA-256 is 32 bytes");
        self.dir.join(format!("{:016x}", u64::from_be_bytes(head)))
    }

    /// Creates session `key`'s directory and the root, each `0700`, refusing one that is a
    /// link or someone else's: in `/tmp`, another user may have made it first.
    pub fn create(&self, key: &str) -> Result<PathBuf> {
        let tmp = self.session(key);
        for dir in [&self.dir, &tmp] {
            private_dir(dir, self.uid)?;
        }
        Ok(tmp)
    }

    /// What session `key`'s Bash may write: its directory, and under `/private` too, since
    /// `/tmp` is a link to `/private/tmp`.
    pub fn writable(&self, key: &str) -> Vec<PathBuf> {
        let tmp = self.session(key);
        let private = self.short.then(|| private_alias(&tmp)).flatten();
        std::iter::once(tmp).chain(private).collect()
    }

    /// `Read` and `Edit` rules for the other sessions' directories and Claude Code's shared
    /// `/tmp/claude-<uid>`, by both of their paths. Under `MOTHERSHIP_HOME` the home
    /// directory's rules already cover them, so there are none.
    pub fn deny_rules(&self, key: &str) -> Vec<String> {
        if !self.short {
            return Vec::new();
        }
        let own = self.session(key);
        let shared = PathBuf::from(format!("/tmp/claude-{}", self.uid));
        let mut rules = crate::sandbox::deny_others(&self.dir, &own);
        let mut denied = vec![shared.clone()];
        if let (Some(root), Some(own), Some(shared)) = (
            private_alias(&self.dir),
            private_alias(&own),
            private_alias(&shared),
        ) {
            rules.extend(crate::sandbox::deny_others(&root, &own));
            denied.push(shared);
        }
        for dir in denied {
            for tool in ["Read", "Edit"] {
                rules.push(format!("{tool}(/{}/**)", dir.display()));
            }
        }
        rules
    }

    /// Fails when a session's directory is too long for what Claude Code makes in it, which
    /// would fail every turn's sandbox or share Bash's `TMPDIR` among sessions instead.
    pub fn check(&self) -> Result<()> {
        self.check_for(cfg!(target_os = "macos"))
    }

    fn check_for(&self, macos: bool) -> Result<()> {
        let tmp = self.session("");
        let len = tmp.as_os_str().len();
        let max = SUN_PATH - 1 - SOCKET_NAME;
        anyhow::ensure!(
            len <= max,
            "a session's TMPDIR ({}) is {len} bytes, over the {max} that leave room for Claude \
             Code's sandbox sockets (sun_path is {SUN_PATH}); move MOTHERSHIP_HOME to a shorter \
             path",
            tmp.display(),
        );
        let child = tmp.join(format!("claude-{}", self.uid));
        let len = child.as_os_str().len();
        anyhow::ensure!(
            !macos || len <= CLAUDE_CHILD_TMPDIR,
            "Claude Code's directory in a session's TMPDIR ({}) is {len} bytes, over the \
             {CLAUDE_CHILD_TMPDIR} past which Bash shares /tmp/claude-{} with every session",
            child.display(),
            self.uid,
        );
        Ok(())
    }
}

/// `/private/tmp/…` for a path under `/tmp`.
fn private_alias(path: &Path) -> Option<PathBuf> {
    let rest = path.strip_prefix("/tmp").ok()?;
    Some(Path::new("/private/tmp").join(rest))
}

/// Makes `dir` a `0700` directory of `uid`'s, or refuses the one there.
fn private_dir(dir: &Path, uid: u32) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            return Err(e).with_context(|| format!("creating {}", dir.display()));
        }
        _ => {}
    }
    let meta = std::fs::symlink_metadata(dir).with_context(|| format!("{}", dir.display()))?;
    anyhow::ensure!(
        meta.file_type().is_dir(),
        "{} is not a directory (a link?); refusing it as a temporary directory",
        dir.display()
    );
    anyhow::ensure!(
        meta.uid() == uid,
        "{} belongs to uid {}, not mothership's {uid}; refusing it as a temporary directory",
        dir.display(),
        meta.uid()
    );
    if meta.mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, Permissions::from_mode(0o700))
            .with_context(|| format!("making {} private", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short() -> TmpRoot {
        TmpRoot::new(Path::new("/Users/me/.mothership"), 502, true)
    }

    #[test]
    fn a_session_directory_has_a_fixed_length_whatever_the_key() {
        let state = Path::new("/home/me/.mothership");
        let root = TmpRoot::new(state, 1000, false);
        let topic = "a very long Zulip topic about something ".repeat(8);
        let zulip = root.session(&format!("zulip:engineering-and-operations:{topic}"));
        let uuid = root.session("cc2ccfbf-3742-4ceb-9124-5295b54792d0");
        assert_eq!(zulip.parent(), Some(state.join("session-tmp").as_path()));
        assert_eq!(zulip.as_os_str().len(), state.as_os_str().len() + 29);
        assert_eq!(zulip.as_os_str().len(), uuid.as_os_str().len());
        assert_ne!(zulip, uuid);
        root.check_for(false).unwrap();
        assert_eq!(root.writable("k"), [root.session("k")]);
        assert_eq!(root.deny_rules("k"), Vec::<String>::new());
    }

    #[test]
    fn on_macos_a_session_directory_is_short_enough_for_claude_code() {
        let root = short();
        let tmp = root.session("k");
        assert_eq!(tmp.parent(), Some(Path::new("/tmp/ms-502")));
        assert_eq!(tmp.as_os_str().len(), 28);
        root.check_for(true).unwrap();
        let private = Path::new("/private").join(tmp.strip_prefix("/").unwrap());
        assert_eq!(root.writable("k"), [tmp, private]);
    }

    #[test]
    fn refuses_a_directory_past_either_limit() {
        // `/` and the name, then `/session-tmp/` and 16 digits: one byte over sun_path's room.
        let deep = PathBuf::from("/").join("d".repeat(SUN_PATH - 60));
        let err = TmpRoot::new(&deep, 0, false).check_for(false).unwrap_err();
        assert!(err.to_string().contains("MOTHERSHIP_HOME"), "{err}");
        let fits = PathBuf::from("/").join("d".repeat(SUN_PATH - 61));
        TmpRoot::new(&fits, 0, false).check_for(false).unwrap();
        // `/tmp/ms-<uid>/<16>/claude-<uid>` is 33 bytes and twice the uid's digits.
        TmpRoot::new(Path::new("/"), 50_123, true)
            .check_for(true)
            .unwrap();
        let err = TmpRoot::new(Path::new("/"), 501_234, true).check_for(true);
        assert!(err.unwrap_err().to_string().contains("/tmp/claude-501234"));
        // The measured fallback: `<home>/session-tmp/<16>` on macOS.
        let home = TmpRoot::new(Path::new("/Users/me/.mothership"), 502, false);
        assert!(home.check_for(true).is_err());
    }

    #[test]
    fn denies_other_sessions_and_the_shared_directory_by_both_paths() {
        let rules = short().deny_rules("k");
        for path in ["/tmp/claude-502", "/private/tmp/claude-502"] {
            assert!(rules.contains(&format!("Read(/{path}/**)")), "{rules:?}");
            assert!(rules.contains(&format!("Edit(/{path}/**)")), "{rules:?}");
        }

        let base = std::env::temp_dir().join(format!("mothership-tmpdeny-{}", std::process::id()));
        let root = TmpRoot {
            dir: base.clone(),
            ..short()
        };
        let (own, other) = (root.session("k"), root.session("other"));
        for dir in [&own, &other] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let rules = root.deny_rules("k");
        let other = other.display();
        assert!(rules.contains(&format!("Read(/{other}/**)")), "{rules:?}");
        assert!(rules.contains(&format!("Edit(/{other}/**)")), "{rules:?}");
        let own = own.display().to_string();
        assert!(!rules.iter().any(|r| r.contains(&own)), "{rules:?}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn creates_private_directories_and_refuses_links_and_others() {
        let base = std::env::temp_dir().join(format!("mothership-tmproot-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let uid = std::fs::metadata(&base).unwrap().uid();
        let mut root = TmpRoot::new(&base, uid, false);
        let tmp = root.create("k").unwrap();
        for dir in [&root.dir, &tmp] {
            assert_eq!(std::fs::metadata(dir).unwrap().mode() & 0o777, 0o700);
        }
        std::fs::set_permissions(&tmp, Permissions::from_mode(0o755)).unwrap();
        root.create("k").unwrap();
        assert_eq!(std::fs::metadata(&tmp).unwrap().mode() & 0o777, 0o700);

        let err = TmpRoot {
            uid: uid + 1,
            ..root.clone()
        }
        .create("k")
        .unwrap_err();
        assert!(err.to_string().contains("belongs to uid"), "{err}");
        let link = base.join("link");
        std::os::unix::fs::symlink(&root.dir, &link).unwrap();
        root.dir = link;
        let err = root.create("k").unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err}");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
