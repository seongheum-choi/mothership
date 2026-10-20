//! Stand-ins for the `claude`, `codex` and `gh` CLIs. Each appends what it was asked to the JSON
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

/// A prompt containing this has the fake claude ask mothership for a Codex run the way the
/// implement skill does: a request in `MOTHERSHIP_RUNS`, then its answer as the result.
pub const HOST_RUN: &str = "[host-run]";
/// Like [`HOST_RUN`], for the repository command named `setup`.
pub const HOST_COMMAND: &str = "[host-command]";

/// A prompt containing this starts a background task, the way an `Agent` or background `Bash`
/// call does, and ends the turn while it runs. The task finishes shortly after; if input is still
/// open then, the fake wakes on its notification and answers [`BACKGROUND_DONE`] in a second
/// result. Closing input first exits the fake with the task lost.
pub const BACKGROUND: &str = "[background]";
/// Like [`BACKGROUND`], with a task that never finishes.
pub const BACKGROUND_HANG: &str = "[background-hang]";
pub const BACKGROUND_DONE: &str = "Background work done.";

/// With `FAKE_HANG` set, the fake starts a tool and then never announces a session, the way
/// Claude Code hangs on a macOS privacy dialog nobody can answer.
pub const HANG: &str = "FAKE_HANG";

/// While this file is next to `FAKE_LOG`, the fake claude answers every prompt the way Claude
/// Code 2.1.296 does when the account's usage limit refuses it. The limit resets as many seconds
/// after the refusal as the file says.
pub const CLAUDE_LIMIT: &str = "claude-limit";

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
    // Not locked: a background task writes its follow-up turn from another thread.
    let mut out = std::io::stdout();
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
        if let Some(resets_at) = limit() {
            refuse(&mut out, resets_at);
            continue;
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
        let result = if text.contains(BACKGROUND) || text.contains(BACKGROUND_HANG) {
            background(&mut out, pid, text.contains(BACKGROUND));
            "Started in the background.".to_string()
        } else if text.contains(HOST_COMMAND) {
            host_run(&json!({"kind": "command", "name": "setup"}))
        } else if text.contains(HOST_RUN) {
            host_run(
                &json!({"kind": "codex", "prompt": text, "schema": {"type": "object"}, "effort": "high"}),
            )
        } else {
            format!("Done: {text}")
        };
        emit(
            &mut out,
            &json!({"type": "result", "subtype": "success", "is_error": false, "result": result}),
        );
    }
    ExitCode::SUCCESS
}

/// The JSONL file next to `FAKE_LOG` that the fake codex plays back, line by line, with
/// `{thread}` and `{prompt}` filled in.
pub const CODEX_SCRIPT: &str = "codex-script.jsonl";
/// With a prompt containing [`HOLD`], the fake codex keeps the turn open until this file
/// appears next to `FAKE_LOG`.
pub const CODEX_RELEASE: &str = "codex-release";

/// `codex exec --json [resume <id>] -`: reads the prompt from stdin to its end, records it with
/// the argument list, then plays back the script. The thread id is `thread-<pid>` on a new
/// thread and the resumed one otherwise.
pub fn codex() -> ExitCode {
    use std::io::Read;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let resume = args
        .windows(2)
        .find(|w| w[0] == "resume")
        .map(|w| w[1].clone());
    let mut prompt = String::new();
    let _ = std::io::stdin().read_to_string(&mut prompt);
    let pid = std::process::id();
    record(&json!({
        "tool": "codex",
        "pid": pid,
        "cwd": std::env::current_dir().unwrap_or_default(),
        "args": args,
        "resume": resume,
        "surface": std::env::var("MOTHERSHIP_SURFACE").ok(),
        "prompt": prompt,
    }));
    let thread = resume.unwrap_or_else(|| format!("thread-{pid}"));
    let log = std::path::PathBuf::from(std::env::var("FAKE_LOG").unwrap_or_default());
    // A run mothership starts for a session: the final message goes to `-o`, as JSON when a
    // schema shapes it.
    if let Some(last) = args.windows(2).find(|w| w[0] == "-o").map(|w| w[1].clone()) {
        if prompt.contains(HOLD) {
            let release = log.with_file_name(CODEX_RELEASE);
            while !release.exists() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        let answer = if args.iter().any(|a| a == "--output-schema") {
            json!({"findings": [], "cwd": std::env::current_dir().unwrap_or_default()}).to_string()
        } else {
            format!("Reviewed: {prompt}")
        };
        return match std::fs::write(last, answer) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        };
    }
    let mut out = std::io::stdout().lock();
    emit(
        &mut out,
        &json!({"type": "thread.started", "thread_id": thread}),
    );
    if prompt.contains(HOLD) {
        let release = log.with_file_name(CODEX_RELEASE);
        while !release.exists() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let script = std::fs::read_to_string(log.with_file_name(CODEX_SCRIPT)).unwrap_or_default();
    // The prompt goes inside a JSON string, so it is escaped the way JSON escapes it.
    let escaped = Value::from(prompt.as_str()).to_string();
    let escaped = &escaped[1..escaped.len() - 1];
    for line in script.lines().filter(|l| !l.trim().is_empty()) {
        let line = line
            .replace("{thread}", &thread)
            .replace("{prompt}", escaped);
        let _ = writeln!(out, "{line}").and_then(|()| out.flush());
    }
    ExitCode::SUCCESS
}

/// Asks mothership for `request` and waits for the answer file.
fn host_run(request: &Value) -> String {
    let runs = std::path::PathBuf::from(std::env::var("MOTHERSHIP_RUNS").unwrap_or_default());
    let tmp = runs.join("r1.tmp");
    if std::fs::write(&tmp, request.to_string())
        .and_then(|()| std::fs::rename(&tmp, runs.join("r1.json")))
        .is_err()
    {
        return format!("cannot ask in {}", runs.display());
    }
    for _ in 0..1000 {
        if let Ok(answer) = std::fs::read_to_string(runs.join("r1.out.json")) {
            return format!("Host: {answer}");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    "Host: no answer".into()
}

/// `sandbox-exec -p <profile> <program> <args>…`: records the profile, then runs the program
/// unconfined with the same stdio and exits as it did.
pub fn sandbox_exec() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [flag, profile, program, rest @ ..] = args.as_slice() else {
        return ExitCode::FAILURE;
    };
    if flag != "-p" {
        return ExitCode::FAILURE;
    }
    record(&json!({"tool": "sandbox-exec", "profile": profile, "program": program}));
    let status = std::process::Command::new(program).args(rest).status();
    match status.ok().and_then(|s| s.code()) {
        Some(0) => ExitCode::SUCCESS,
        Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        None => ExitCode::FAILURE,
    }
}

/// The `resetsAt` of the usage limit [`CLAUDE_LIMIT`] sets, while that file is there.
fn limit() -> Option<u64> {
    let log = std::path::PathBuf::from(std::env::var("FAKE_LOG").ok()?);
    let after = std::fs::read_to_string(log.with_file_name(CLAUDE_LIMIT)).ok()?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    Some(now.ok()?.as_secs() + after.trim().parse::<u64>().ok()?)
}

/// The refused request's API error message and the result that ends the turn on it.
fn refuse(out: &mut impl Write, resets_at: u64) {
    let text = "You've hit your session limit";
    emit(
        out,
        &json!({
            "type": "assistant",
            "error": "rate_limit",
            "is_api_error_message": true,
            "api_error_status": 429,
            "api_error": "usage_limit_reached",
            "api_error_params": {"rate_limit_info": {
                "status": "rejected",
                "isUsingOverage": false,
                "rateLimitType": "five_hour",
                "resetsAt": resets_at,
            }},
            "parent_tool_use_id": null,
            "message": {"content": [{"type": "text", "text": text}]},
        }),
    );
    emit(
        out,
        &json!({"type": "result", "subtype": "success", "is_error": true, "api_error_status": 429, "api_error": "usage_limit_reached", "result": text}),
    );
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
    // One write, so a background task's lines never split the main turn's.
    // A closed pipe means mothership is gone; nothing is left to answer.
    let _ = out
        .write_all(format!("{line}\n").as_bytes())
        .and_then(|()| out.flush());
}

/// Starts a background task with the events Claude Code 2.1.304 writes for one. When `finishes`,
/// a thread reports it done after a moment and runs the turn its notification wakes.
fn background(out: &mut impl Write, pid: u32, finishes: bool) {
    let task = json!({"task_id": "t1", "task_type": "local_agent", "description": "Build"});
    emit(
        out,
        &json!({"type": "system", "subtype": "background_tasks_changed", "tasks": [task]}),
    );
    if !finishes {
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut out = std::io::stdout();
        for line in [
            json!({"type": "system", "subtype": "task_notification", "task_id": "t1", "status": "completed"}),
            json!({"type": "system", "subtype": "background_tasks_changed", "tasks": []}),
            json!({"type": "system", "subtype": "init", "session_id": format!("fake-{pid}")}),
            json!({"type": "result", "subtype": "success", "is_error": false, "result": BACKGROUND_DONE}),
        ] {
            emit(&mut out, &line);
        }
    });
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
