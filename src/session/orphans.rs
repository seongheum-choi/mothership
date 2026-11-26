//! Processes an ended session left running, a double-forked dev server among them: found by
//! the session's progress file in their environment, since they may have left both the agent's
//! process tree and its process group, and killed.

use crate::app::App;
use std::process::{Command, Stdio};

/// Kills every process of this user whose environment names the progress file of one of the
/// sessions `keys` (see [`App::session_dir`]), and logs what it killed.
// shortcut: a process that drops `MOTHERSHIP_PROGRESS_FILE` from its environment still
// survives; a per-run user or launchd job would catch it, if an agent ever does that.
pub fn kill_left<'a>(app: &App, keys: impl IntoIterator<Item = &'a str>) {
    let keys: Vec<&str> = keys.into_iter().collect();
    if keys.is_empty() {
        return;
    }
    let needles: Vec<String> = keys
        .iter()
        .map(|k| {
            let file = app.session_dir(k).join("progress.log");
            format!("MOTHERSHIP_PROGRESS_FILE={}", file.display())
        })
        .collect();
    // `x` lists processes without a terminal too, `e` appends each one's environment; the same
    // flags work on procps and macOS.
    let listed = Command::new("ps")
        .args(["exww", "-o", "pid=,command="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let listed = match listed {
        Ok(out) => String::from_utf8_lossy(&out.stdout).into_owned(),
        Err(e) => {
            tracing::warn!("listing processes to find what ended sessions left: {e}");
            return;
        }
    };
    for (pid, i) in matching(&listed, &needles) {
        if pid == std::process::id() {
            continue;
        }
        let killed = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if killed {
            tracing::info!("[{}] killed process {pid} it left running", keys[i]);
        }
    }
}

/// The pids in `ps` output (`<pid> <command> <environment>` per line) with one of `needles`
/// as a whole word, each with the index of the needle it has.
fn matching(ps: &str, needles: &[String]) -> Vec<(u32, usize)> {
    ps.lines()
        .filter_map(|line| {
            let (pid, rest) = line.trim_start().split_once(' ')?;
            let words: Vec<&str> = rest.split(' ').collect();
            let i = needles.iter().position(|n| words.contains(&n.as_str()))?;
            Some((pid.parse().ok()?, i))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_needle_only_as_a_whole_word() {
        let needles = vec![
            "V=/p/a/progress.log".to_string(),
            "V=/p/b/progress.log".into(),
        ];
        let ps = "  12 node server.ts HOME=/h V=/p/a/progress.log X=1\n\
                  13 node V=/p/a/progress.log.old\n\
                  14 sleep 9 V=/p/b/progress.log\n\
                  15 sleep 9 W=/p/b/progress.log\n\
                  junk";
        assert_eq!(matching(ps, &needles), vec![(12, 0), (14, 1)]);
    }

    #[test]
    fn ps_shows_a_double_forked_process_with_its_environment() {
        let needle = format!("MOTHERSHIP_TEST_ORPHAN={}", std::process::id());
        // The subshell's `&` hands `sleep` to init, out of this process's tree.
        Command::new("sh")
            .args(["-c", "(sleep 30 >/dev/null 2>&1 &)"])
            .env("MOTHERSHIP_TEST_ORPHAN", std::process::id().to_string())
            .status()
            .unwrap();
        let out = Command::new("ps")
            .args(["exww", "-o", "pid=,command="])
            .output()
            .unwrap();
        let found = matching(&String::from_utf8_lossy(&out.stdout), &[needle]);
        assert_eq!(found.len(), 1, "{found:?}");
        Command::new("kill")
            .args(["-KILL", &found[0].0.to_string()])
            .status()
            .unwrap();
    }
}
