//! Claude Code's usage limit: while it lasts, turns that would run Claude Code run on Codex
//! instead, unless a message chose the runner, and the first turn after it resets goes back to
//! Claude Code.

use super::Runner;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Whether a turn runs on Codex rather than `configured`: only one that would run Claude Code
/// switches, and only until the limit resets at `resets_at` (Unix seconds).
pub fn applies(configured: Runner, resets_at: u64, now: u64) -> bool {
    configured == Runner::Claude && now < resets_at
}

/// Added to the failure of a turn that hit the limit on a session `[runner=claude]` keeps on
/// Claude Code.
pub const PINNED: &str = "This session is pinned to Claude Code with `[runner=claude]`, so it \
     does not move to Codex. Reply with `[runner=default]` or `[runner=codex]` to let it.";

/// Whether the Codex CLI at `bin` is there to switch to: a path to a file, or a name found on
/// `path`, the `PATH` the agent process gets, so the check sees what the launch will. `None`
/// falls back to this process's `PATH`, which the agent inherits when `AGENT_ENV` sets none.
pub fn installed(bin: &str, path: Option<&str>) -> bool {
    locate(bin, path).is_some()
}

/// Where the CLI `bin` is, looked for as [`installed`] does.
pub fn locate(bin: &str, path: Option<&str>) -> Option<PathBuf> {
    if bin.contains('/') {
        return Path::new(bin).is_file().then(|| PathBuf::from(bin));
    }
    let path = path
        .map(OsString::from)
        .or_else(|| std::env::var_os("PATH"))?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|file| file.is_file())
}

/// The reset to keep when a turn reports one at `reported`: the later of the two. Two sessions
/// hitting the limit at once would otherwise leave whichever wrote last, shortening the wait.
pub fn later(current: Option<u64>, reported: u64) -> u64 {
    current.map_or(reported, |at| at.max(reported))
}

/// The thought that opens a turn moved to Codex as Claude Code hit its limit.
pub fn switched(resets_at: Option<u64>) -> String {
    let until = resets_at.map_or_else(String::new, |t| format!(" until {}", utc(t)));
    format!("Claude usage limit reached → switching to Codex{until}")
}

/// The thought that opens a later turn that runs on Codex while the limit lasts.
pub fn still_limited(resets_at: u64) -> String {
    format!(
        "Claude usage limit until {} → running on Codex",
        utc(resets_at)
    )
}

/// Unix seconds as `2026-10-12 14:05 UTC`.
fn utc(secs: u64) -> String {
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's civil_from_days, for days since 1970-01-01 that are not negative.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year}-{month:02}-{day:02} {:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_to_claude_until_the_reset() {
        assert!(applies(Runner::Claude, 100, 99));
        assert!(!applies(Runner::Claude, 100, 100));
        assert!(!applies(Runner::Codex, 100, 99));
    }

    #[test]
    fn formats_the_reset_in_utc() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(1_791_900_300), "2026-10-13 14:05 UTC");
        assert_eq!(utc(951_825_600), "2000-02-29 12:00 UTC");
        assert_eq!(
            switched(Some(0)),
            "Claude usage limit reached → switching to Codex until 1970-01-01 00:00 UTC"
        );
        assert_eq!(
            switched(None),
            "Claude usage limit reached → switching to Codex"
        );
    }

    #[test]
    fn finds_binaries_by_path_or_name() {
        assert!(installed("/bin/sh", None));
        assert!(installed("sh", None));
        assert!(!installed("/nonexistent/codex", None));
        assert!(!installed("mothership-no-such-binary", None));
    }

    #[test]
    fn looks_for_a_bare_name_on_the_agents_path() {
        assert!(installed("sh", Some("/nowhere:/bin")));
        assert!(!installed("sh", Some("/nowhere")));
        // A path to a file is not looked for on any `PATH`.
        assert!(installed("/bin/sh", Some("/nowhere")));
    }

    #[test]
    fn keeps_the_later_reset() {
        assert_eq!(later(None, 100), 100);
        assert_eq!(later(Some(200), 100), 200);
        assert_eq!(later(Some(100), 200), 200);
    }
}
