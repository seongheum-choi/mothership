//! Stand-ins for the `claude` and `gh` CLIs. Both append what they were asked to the JSON
//! lines file `FAKE_LOG`, which mothership's environment passes down to them.

use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    process::ExitCode,
};

/// A prompt containing this keeps the turn open: later prompts are read but not answered
/// until one contains [`RELEASE`], so tests can add prompts to a running turn or stop it.
/// While it holds, the fake also runs a `sleep` in its own process group, the way Claude Code
/// runs shell commands, so a stop must kill the whole tree, not just close stdin.
pub const HOLD: &str = "[hold]";
pub const RELEASE: &str = "[release]";
/// A prompt containing this has a shell command append [`PROGRESS_LINE`] to
/// `MOTHERSHIP_PROGRESS_FILE`, the way a skill reports from inside Bash.
pub const PROGRESS: &str = "[progress]";
pub const PROGRESS_LINE: &str = "Phase 1 of 2";

/// With `FAKE_HANG` set, the fake starts a tool and then never announces a session, the way
/// Claude Code hangs on a macOS privacy dialog nobody can answer.
pub const HANG: &str = "FAKE_HANG";

/// `claude -p --input-format stream-json --output-format stream-json`: announces a session,
/// then answers each prompt with a tool call, a thought and `Done: <prompt>` as the result.
/// Each prompt is recorded with the full argument list.
pub fn claude() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let resume = args
        .windows(2)
        .find(|w| w[0] == "--resume")
        .map(|w| w[1].clone());
    let pid = std::process::id();
    let cwd = std::env::current_dir().unwrap_or_default();
    if std::env::var_os(HANG).is_some() {
        let tool = busy_tool();
        record(&json!({
            "tool": "claude",
            "pid": pid,
            "args": args,
            "tool_pid": tool.as_ref().map(std::process::Child::id),
        }));
        loop {
            std::thread::park();
        }
    }
    let mut out = std::io::stdout().lock();
    emit(
        &mut out,
        &json!({"type": "system", "subtype": "init", "session_id": format!("fake-{pid}")}),
    );
    let mut holding = None;
    let mut tool: Option<std::process::Child> = None;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let text = message["message"]["content"].as_str().unwrap_or_default();
        if tool.is_none() && text.contains(HOLD) {
            tool = busy_tool();
        }
        record(&json!({
            "tool": "claude",
            "pid": pid,
            "cwd": cwd,
            "args": args,
            "resume": resume,
            "surface": std::env::var("MOTHERSHIP_SURFACE").ok(),
            "prompt": text,
            "tool_pid": tool.as_ref().map(std::process::Child::id),
        }));
        if *holding.get_or_insert(text.contains(HOLD)) && !text.contains(RELEASE) {
            continue;
        }
        holding = Some(false);
        // Only a released hold ends its tool; on stdin closing it stays up, so only mothership
        // killing the process tree ends it.
        if let Some(mut child) = tool.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let assistant = |content: Value| json!({"type": "assistant", "message": {"content": [content]}, "parent_tool_use_id": null});
        emit(
            &mut out,
            &assistant(json!({"type": "tool_use", "name": "Bash", "input": {"command": "true"}})),
        );
        if text.contains(PROGRESS) {
            let _ = std::process::Command::new("sh")
                .args([
                    "-c",
                    &format!("echo '{PROGRESS_LINE}' >> \"$MOTHERSHIP_PROGRESS_FILE\""),
                ])
                .status();
        }
        emit(
            &mut out,
            &assistant(json!({"type": "text", "text": "Looking into it."})),
        );
        emit(
            &mut out,
            &json!({"type": "result", "subtype": "success", "is_error": false, "result": format!("Done: {text}")}),
        );
    }
    ExitCode::SUCCESS
}

fn busy_tool() -> Option<std::process::Child> {
    use std::os::unix::process::CommandExt;
    std::process::Command::new("sleep")
        .arg("120")
        .process_group(0)
        .spawn()
        .ok()
}

/// `gh api repos/<owner>/<name>/pulls/<n>`: a pull request whose head is `FAKE_GH_HEAD_REF` in
/// `FAKE_GH_HEAD_REPO`, by default the same repository. Anything else fails.
pub fn gh() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    record(&json!({ "tool": "gh", "args": args }));
    let pull = match args.as_slice() {
        [api, path] if api == "api" => pull_request(path),
        _ => None,
    };
    let Some((repo, number)) = pull else {
        eprintln!("fake gh: unsupported command {args:?}");
        return ExitCode::FAILURE;
    };
    let head = std::env::var("FAKE_GH_HEAD_REF").unwrap_or_default();
    let head_repo = std::env::var("FAKE_GH_HEAD_REPO").unwrap_or(repo);
    println!(
        "{}",
        json!({"number": number, "head": {"ref": head, "repo": {"full_name": head_repo}}})
    );
    ExitCode::SUCCESS
}

/// `repos/o/r/pulls/7` as `("o/r", 7)`.
fn pull_request(path: &str) -> Option<(String, u64)> {
    match path.split('/').collect::<Vec<_>>()[..] {
        ["repos", owner, name, "pulls", number] => {
            Some((format!("{owner}/{name}"), number.parse().ok()?))
        }
        _ => None,
    }
}

fn emit(out: &mut impl Write, line: &Value) {
    // A closed pipe means mothership is gone; nothing is left to answer.
    let _ = writeln!(out, "{line}").and_then(|()| out.flush());
}

/// One line per call, written at once so concurrent fakes do not interleave.
fn record(entry: &Value) {
    let Ok(path) = std::env::var("FAKE_LOG") else {
        return;
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    if let Ok(mut file) = file {
        let _ = file.write_all(format!("{entry}\n").as_bytes());
    }
}
