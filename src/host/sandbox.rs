//! The `sandbox` request: a command for a disposable container, the limits it runs under and
//! the scratch files it keeps; the container it gets, which `container.rs` starts: no network
//! but its own loopback, a read-only root and workspace, a fresh scratch tmpfs as the only
//! place to write, those limits, an environment of the image's own and [`ENV`], and nothing of
//! the host's (home, credentials, sockets); and what the system prompt says about it.

use super::limits::{Limits, requested};
use serde_json::Value;
use std::path::Path;

/// How many files a request may name in `artifacts`.
const MAX_ARTIFACTS: usize = 32;
/// The environment the container gets besides its image's own: everything a tool writes
/// unasked goes to the scratch.
const ENV: [&str; 4] = [
    "HOME=/scratch/home",
    "TMPDIR=/scratch/tmp",
    "XDG_CACHE_HOME=/scratch/cache",
    "CARGO_TARGET_DIR=/scratch/target",
];
/// `nobody`: not root, so `/dev` and the files the runtime mounts in `/etc` are not writable.
const USER: &str = "65534:65534";

#[derive(Debug, PartialEq)]
pub struct SandboxRun {
    /// Run with `/bin/sh -c` in the read-only workspace.
    pub command: String,
    pub limits: Limits,
    /// Files at the top of the scratch directory to keep after the run.
    pub artifacts: Vec<String>,
}

/// `{"kind": "sandbox", "command": …, "limits"?: {…}, "artifacts"?: […]}`, each limit at
/// most `ceiling`'s.
pub fn parse(value: &Value, ceiling: &Limits) -> Result<SandboxRun, String> {
    let command = value["command"]
        .as_str()
        .filter(|c| !c.trim().is_empty())
        .ok_or("`command` must be a non-empty string")?;
    let artifacts = match &value["artifacts"] {
        Value::Null => Vec::new(),
        Value::Array(names) if names.len() <= MAX_ARTIFACTS => names
            .iter()
            .map(|n| n.as_str().filter(|n| artifact_name(n)).map(String::from))
            .collect::<Option<_>>()
            .ok_or("`artifacts` must name files at the top of the scratch directory")?,
        _ => {
            return Err(format!(
                "`artifacts` must be a list of at most {MAX_ARTIFACTS} names"
            ));
        }
    };
    Ok(SandboxRun {
        command: command.to_string(),
        limits: requested(&value["limits"], ceiling)?,
        artifacts,
    })
}

/// A plain file name: it becomes part of the promoted file's name next to the request.
fn artifact_name(name: &str) -> bool {
    !name.starts_with('.')
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `run` arguments starting container `name` idle, for the request's command to be run in it.
pub fn run_args(
    name: &str,
    image: &str,
    workspace: &Path,
    l: &Limits,
) -> Result<Vec<String>, String> {
    let ws = (workspace.to_str())
        .filter(|w| !w.contains([',', '"']))
        .ok_or("the workspace's path cannot be given to --mount")?;
    let mut args: Vec<String> = [
        "run",
        "--detach",
        "--name",
        name,
        "--pull",
        "never",
        "--network",
        "none",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--user",
        USER,
        "--hostname",
        "sandbox",
        "--workdir",
        "/work",
    ]
    .map(String::from)
    .to_vec();
    let memory = format!("{}m", l.memory_mb);
    let fsize = l.file_size_mb.saturating_mul(1 << 20);
    args.extend([
        "--memory".into(),
        memory.clone(),
        // The same as `--memory`: no swap to spill into.
        "--memory-swap".into(),
        memory,
        "--cpus".into(),
        l.cpus.to_string(),
        "--pids-limit".into(),
        l.pids.to_string(),
        "--ulimit".into(),
        format!("fsize={fsize}:{fsize}"),
        "--tmpfs".into(),
        format!(
            "/scratch:rw,exec,nosuid,nodev,size={}m,mode=1777",
            l.disk_mb
        ),
        "--mount".into(),
        format!("type=bind,src={ws},dst=/work,readonly"),
    ]);
    // The runtime's own `/dev/shm` and `/dev/mqueue` are writable by anyone; a mount at the same
    // place replaces them. nerdctl has no `--ipc none` to leave them out.
    for dir in ["/dev/shm", "/dev/mqueue"] {
        args.extend(["--mount".into(), format!("type=tmpfs,dst={dir},readonly")]);
    }
    for var in ENV {
        args.extend(["--env".into(), var.into()]);
    }
    // Longer than any run may take; the container is removed when the run ends.
    args.extend(["--entrypoint", "sleep", image, "86400"].map(String::from));
    Ok(args)
}

/// What the session's system prompt says when its repository has a `sandbox_image`, whose
/// ceiling is `c`.
pub fn prompt(c: &Limits) -> String {
    format!(
        "To run this repository's code (tests, a proof of concept, a database) without network \
         or access to anything but the workspace, write `{{\"kind\": \"sandbox\", \"command\": \
         \"<sh command>\", \"limits\": {{…}}, \"artifacts\": [\"<file>\"]}}` to a file in \
         `$MOTHERSHIP_RUNS` under a temporary name, rename it to `<id>.json`, and wait for \
         `<id>.out.json`. mothership runs the command with `/bin/sh -c` in a fresh container \
         from the repository's image: no network but its own loopback, the workspace read-only \
         at `/work` (the working directory), only `/scratch` writable, and an environment of \
         the image's own variables and `{}`, `{}`, `{}` and `{}`, nothing of mothership's, the \
         session's or yours; the image holds the toolchains and dependency caches, and nothing \
         is downloaded. `limits` may lower `memory_mb` \
         ({}), `cpus` ({}), `pids` ({}), `file_size_mb` ({}), `disk_mb` ({}, the size of \
         `/scratch`) and `timeout_secs` ({}). `artifacts` names files at the top of \
         `/scratch` to keep; the rest of `/scratch` is gone after the run. Send the output of what \
         you leave in the background (a database) to a file, or the run waits for it until its \
         timeout. The answer is \
         `{{\"result\": {{\"exit\": <code or null>, \"timed_out\": …, \"output\": <last lines>, \
         \"log\": <whole output>, \"artifacts\": {{<name>: <path>}}, \"skipped\": {{<name>: \
         <why>}}}}}}`; `{{\"error\": …}}` means it could not run, so there is no sandbox for \
         this work.\n\n",
        ENV[0],
        ENV[1],
        ENV[2],
        ENV[3],
        c.memory_mb,
        c.cpus,
        c.pids,
        c.file_size_mb,
        c.disk_mb,
        c.timeout_secs
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::limits::CEILING;
    use serde_json::json;

    #[test]
    fn takes_lower_limits_and_defaults_to_the_ceiling() {
        let run = parse(
            &json!({
                "command": "cargo test",
                "limits": {"memory_mb": 512, "cpus": 0.5, "timeout_secs": 60},
                "artifacts": ["report.json", "out-1.txt"],
            }),
            &CEILING,
        )
        .unwrap();
        assert_eq!(run.command, "cargo test");
        assert_eq!(
            run.limits,
            Limits {
                memory_mb: 512,
                cpus: 0.5,
                timeout_secs: 60,
                ..CEILING
            }
        );
        assert_eq!(run.artifacts, ["report.json", "out-1.txt"]);
        let bare = parse(&json!({"command": "true"}), &CEILING).unwrap();
        assert_eq!(
            (bare.limits, bare.artifacts),
            (CEILING, Vec::<String>::new())
        );
    }

    #[test]
    fn prompt_names_every_control() {
        let p = prompt(&CEILING);
        for control in [
            "no network but its own loopback",
            "the workspace read-only at `/work`",
            "only `/scratch` writable",
            "an environment of the image's own variables and `HOME=/scratch/home`, \
             `TMPDIR=/scratch/tmp`, `XDG_CACHE_HOME=/scratch/cache` and \
             `CARGO_TARGET_DIR=/scratch/target`, nothing of mothership's, the session's or yours",
            "`memory_mb` (4096)",
            "`cpus` (2)",
            "`pids` (512)",
            "`file_size_mb` (1024)",
            "`disk_mb` (2048, the size of `/scratch`)",
            "`timeout_secs` (1800)",
        ] {
            assert!(p.contains(control), "{control:?} in {p}");
        }
    }

    #[test]
    fn starts_the_container_closed() {
        let l = Limits {
            memory_mb: 256,
            cpus: 0.5,
            pids: 32,
            file_size_mb: 8,
            disk_mb: 64,
            timeout_secs: 60,
        };
        let args = run_args("c1", "img:1", Path::new("/w/SH-1"), &l).unwrap();
        for pair in [
            ["--network", "none"],
            ["--pull", "never"],
            ["--cap-drop", "ALL"],
            ["--user", USER],
            ["--memory", "256m"],
            ["--memory-swap", "256m"],
            ["--cpus", "0.5"],
            ["--pids-limit", "32"],
            ["--ulimit", "fsize=8388608:8388608"],
            [
                "--tmpfs",
                "/scratch:rw,exec,nosuid,nodev,size=64m,mode=1777",
            ],
            ["--mount", "type=bind,src=/w/SH-1,dst=/work,readonly"],
            ["--env", "HOME=/scratch/home"],
        ] {
            assert!(args.windows(2).any(|w| w == pair), "{pair:?} in {args:?}");
        }
        assert!(args.contains(&"--read-only".into()));
        assert_eq!(args[args.len() - 2..], ["img:1", "86400"]);
        // No other way in: the workspace read-only, and empty read-only directories.
        let mounts: Vec<&str> = (args.windows(2))
            .filter(|w| w[0] == "--mount")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(
            mounts,
            [
                "type=bind,src=/w/SH-1,dst=/work,readonly",
                "type=tmpfs,dst=/dev/shm,readonly",
                "type=tmpfs,dst=/dev/mqueue,readonly",
            ]
        );
        for way_in in ["-v", "--volume", "--volumes-from", "--privileged", "--ipc"] {
            assert!(!args.contains(&way_in.into()), "{way_in}");
        }
        assert!(run_args("c1", "img:1", Path::new("/w/a,src=/"), &l).is_err());
    }

    #[test]
    fn refuses_limits_over_the_ceiling_and_paths_out_of_scratch() {
        for bad in [
            json!({}),
            json!({"command": " "}),
            json!({"command": "x", "limits": {"memory_mb": CEILING.memory_mb + 1}}),
            json!({"command": "x", "limits": {"pids": 0}}),
            json!({"command": "x", "limits": {"cpus": 3}}),
            json!({"command": "x", "limits": {"timeout_secs": "60"}}),
            json!({"command": "x", "limits": {"swap_mb": 1}}),
            json!({"command": "x", "limits": []}),
            json!({"command": "x", "artifacts": ["../etc/passwd"]}),
            json!({"command": "x", "artifacts": ["a/b"]}),
            json!({"command": "x", "artifacts": [".."]}),
            json!({"command": "x", "artifacts": [7]}),
            json!({"command": "x", "artifacts": "a"}),
            json!({"command": "x", "artifacts": vec!["a"; MAX_ARTIFACTS + 1]}),
        ] {
            assert!(parse(&bad, &CEILING).is_err(), "{bad}");
        }
    }
}
