//! Runs a `sandbox` request in a disposable container (see `sandbox.rs` for what it is
//! allowed) through `CONTAINER_RUNTIME`, a Docker-compatible CLI: starting it, running the
//! command in it within its time limit, copying out only the files the request names, and
//! removing it however the run ends.

use super::{
    Host,
    process::{keep_end, tail},
    request,
    sandbox::{SandboxRun, run_args},
};
use serde_json::{Map, Value, json};
use std::{process::Stdio, time::Duration};
use tokio::process::Command;

/// Of mothership's environment, what the runtime CLI gets to find its daemon. The container
/// gets none of it.
const RUNTIME_ENV: [&str; 6] = [
    "PATH",
    "HOME",
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_CONFIG",
    "XDG_RUNTIME_DIR",
];
/// The most one artifact, and all of a run's together, may take.
const PER_FILE: u64 = 16 << 20;
const TOTAL: u64 = 64 << 20;
/// How long starting the container, or reading one artifact, may take.
const STEP: Duration = Duration::from_mins(2);
/// Makes the scratch directories, then runs the command (`$1`) with stderr interleaved.
const SCRIPT: &str = "mkdir -p \"$HOME\" \"$TMPDIR\" \"$XDG_CACHE_HOME\" || exit 125\n\
                      exec 2>&1\nexec /bin/sh -c \"$1\"";

impl Host {
    /// Runs `run`'s command in a fresh container and promotes its artifacts next to request
    /// `id`. An error means it could not run at all.
    pub(super) async fn sandbox(&self, id: &str, run: &SandboxRun) -> Result<Value, String> {
        let image = self.sandbox_image.as_deref().ok_or(
            "this repository has no `sandbox_image` in repos.json, so nothing runs in a sandbox",
        )?;
        let name = format!("mothership-sandbox-{}", request::unique());
        let args = run_args(&name, image, &self.cwd, &run.limits)?;
        // Before it starts, so a run cut short at any point still removes it.
        let _container = Container {
            runtime: self.container_runtime.clone(),
            name: name.clone(),
        };
        let started = tokio::time::timeout(STEP, self.runtime().args(&args).output())
            .await
            .map_err(|_| "the container did not start within two minutes".to_string())?
            .map_err(|e| format!("cannot start {}: {e}", self.container_runtime))?;
        if !started.status.success() {
            return Err(format!(
                "cannot start a container from {image}: {}",
                tail(&started.stderr, 5)
            ));
        }
        let mut child = (self.runtime())
            .args(["exec", &name, "/bin/sh", "-c", SCRIPT, "sh", &run.command])
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", self.container_runtime))?;
        let output = tokio::spawn(keep_end(child.stdout.take()));
        // Only the CLI's own complaints: the command's stderr is in its stdout.
        let complaints = tokio::spawn(keep_end(child.stderr.take()));
        let limit = Duration::from_secs(run.limits.timeout_secs);
        let waited = tokio::time::timeout(limit, child.wait()).await;
        let status = waited.ok().transpose().map_err(|e| e.to_string())?;
        if status.is_none() {
            // The command goes on in the container until it is removed; the CLI stops here.
            let _ = child.start_kill();
        }
        let mut output = output.await.unwrap_or_default();
        output.extend(complaints.await.unwrap_or_default());
        let log = request::log(&self.runs, id);
        let log = match request::write_new(&log, &output) {
            Ok(()) => log.display().to_string(),
            Err(e) => format!("not kept: {e}"),
        };
        let (artifacts, skipped) = if status.is_some() {
            self.promote(&name, id, &run.artifacts).await
        } else {
            let why = |a: &String| (a.clone(), Value::from("the run timed out"));
            (Map::new(), run.artifacts.iter().map(why).collect())
        };
        Ok(json!({
            "exit": status.and_then(|s| s.code()),
            "timed_out": status.is_none(),
            "output": tail(&output, 20),
            "log": log,
            "artifacts": artifacts,
            "skipped": skipped,
        }))
    }

    /// Copies each of `names` out of container `name`'s scratch to `<id>.artifact.<name>`
    /// beside the request, within [`PER_FILE`] and [`TOTAL`]; says why for those it skips.
    async fn promote(
        &self,
        name: &str,
        id: &str,
        names: &[String],
    ) -> (Map<String, Value>, Map<String, Value>) {
        let (mut kept, mut skipped) = (Map::new(), Map::new());
        let mut left = TOTAL;
        for file in names {
            let cap = PER_FILE.min(left);
            let fetched = self.fetch(name, file, cap).await.and_then(|bytes| {
                let path = self.runs.join(format!("{id}.artifact.{file}"));
                request::write_new(&path, &bytes).map_err(|e| e.to_string())?;
                Ok((path, bytes.len() as u64))
            });
            match fetched {
                Ok((path, len)) => {
                    left -= len;
                    kept.insert(file.clone(), path.display().to_string().into());
                }
                Err(why) => {
                    skipped.insert(file.clone(), why.into());
                }
            }
        }
        (kept, skipped)
    }

    /// `/scratch/<file>` in container `name` when it is a plain file of at most `cap` bytes.
    async fn fetch(&self, name: &str, file: &str, cap: u64) -> Result<Vec<u8>, String> {
        let script =
            "[ -f \"$1\" ] || { echo not a file >&2; exit 1; }\nexec head -c \"$2\" -- \"$1\"";
        let path = format!("/scratch/{file}");
        let mut command = self.runtime();
        command.args([
            "exec",
            name,
            "/bin/sh",
            "-c",
            script,
            "sh",
            &path,
            &(cap + 1).to_string(),
        ]);
        let out = tokio::time::timeout(STEP, command.output())
            .await
            .map_err(|_| "not read within two minutes".to_string())?
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(tail(&out.stderr, 1));
        }
        if out.stdout.len() as u64 > cap {
            return Err(if cap < PER_FILE {
                format!("past the {} MiB all artifacts may take", TOTAL >> 20)
            } else {
                format!("larger than {} MiB", PER_FILE >> 20)
            });
        }
        Ok(out.stdout)
    }

    /// The runtime CLI with only [`RUNTIME_ENV`], its output read, killed when dropped.
    fn runtime(&self) -> Command {
        let mut command = Command::new(&self.container_runtime);
        command
            .env_clear()
            .envs(
                RUNTIME_ENV
                    .iter()
                    .filter_map(|k| Some((k, std::env::var_os(k)?))),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

/// A run's container, removed with whatever still runs in it when dropped: the run ended, timed
/// out, or the turn stopped.
struct Container {
    runtime: String,
    name: String,
}

impl Drop for Container {
    // shortcut: blocks a tokio worker for as long as `rm --force` takes (well under a second
    // with a healthy daemon); spawn it instead if a slow daemon ever stalls other sessions.
    fn drop(&mut self) {
        let _ = std::process::Command::new(&self.runtime)
            .args(["rm", "--force", &self.name])
            .env_clear()
            .envs(
                RUNTIME_ENV
                    .iter()
                    .filter_map(|k| Some((k, std::env::var_os(k)?))),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{
        CodexHome,
        sandbox::{CEILING, Limits},
    };
    use std::path::PathBuf;

    /// A session's runs directory and a workspace with a file in it, for a repository with
    /// `image`.
    fn host(name: &str, image: Option<&str>, runtime: &str) -> Host {
        let base =
            std::env::temp_dir().join(format!("mothership-ct-{name}-{}", std::process::id()));
        for dir in ["ws", "runs", "run-root"] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        std::fs::write(base.join("ws/source.txt"), "source").unwrap();
        let base = std::fs::canonicalize(base).unwrap();
        Host {
            runs: base.join("runs"),
            run_root: base.join("run-root"),
            progress: std::fs::File::create(base.join("progress.log")).unwrap(),
            cwd: base.join("ws"),
            settings: Value::Null,
            codex_settings: Value::Null,
            env: vec![("SESSION_SECRET".into(), "s3cret".into())],
            commands: std::collections::BTreeMap::new(),
            codex: PathBuf::from("codex"),
            codex_home: CodexHome {
                home: base.join("codex"),
                state: base.join("state"),
            },
            sandbox: String::new(),
            sandbox_image: image.map(String::from),
            container_runtime: runtime.into(),
        }
    }

    fn run(command: &str, limits: Limits, artifacts: &[&str]) -> SandboxRun {
        SandboxRun {
            command: command.into(),
            limits,
            artifacts: artifacts.iter().map(|a| (*a).to_string()).collect(),
        }
    }

    #[tokio::test]
    async fn refuses_a_repository_without_an_image_and_a_missing_runtime() {
        let r = run("true", CEILING, &[]);
        let unset = host("unset", None, "docker").sandbox("a", &r).await;
        assert!(unset.unwrap_err().contains("sandbox_image"));
        let missing = host("missing", Some("img"), "/nonexistent/docker");
        assert!(
            missing
                .sandbox("a", &r)
                .await
                .unwrap_err()
                .contains("cannot start")
        );
    }

    /// A host whose runtime has the test image, or `None` (with why) where it cannot run here:
    /// no runtime, its socket out of reach, or the image not pulled. `MOTHERSHIP_TEST_SANDBOX_IMAGE`
    /// names the image (default `busybox`); it needs `sh`, `head`, `mkdir`, `sleep`, `seq` and
    /// `wget`.
    fn docker(name: &str) -> Option<Host> {
        let image = std::env::var("MOTHERSHIP_TEST_SANDBOX_IMAGE").unwrap_or("busybox".into());
        let inspected = std::process::Command::new("docker")
            .args(["image", "inspect", &image])
            .output();
        let why = match inspected {
            Ok(out) if out.status.success() => return Some(host(name, Some(&image), "docker")),
            Ok(out) => String::from_utf8_lossy(&out.stderr).trim().to_string(),
            Err(e) => format!("no docker: {e}"),
        };
        // CI pulls the image, so there a skip would only hide that these stopped running.
        assert!(std::env::var_os("CI").is_none(), "no sandbox on CI: {why}");
        eprintln!("skipped: {why}");
        None
    }

    async fn result(h: &Host, r: &SandboxRun) -> Value {
        h.sandbox("r1", r).await.unwrap()
    }

    fn output(v: &Value) -> Vec<String> {
        let log = std::fs::read_to_string(v["log"].as_str().unwrap()).unwrap();
        log.lines().map(String::from).collect()
    }

    #[tokio::test]
    async fn reaches_no_other_host() {
        let Some(h) = docker("net") else { return };
        let r = run(
            "echo \"nets: $(ls /sys/class/net | tr '\\n' ' ')\"\n\
             wget -q -T 3 -O /dev/null http://1.1.1.1 && echo reached",
            CEILING,
            &[],
        );
        let lines = output(&result(&h, &r).await);
        assert!(lines.contains(&"nets: lo ".into()), "{lines:?}");
        assert!(!lines.contains(&"reached".into()), "{lines:?}");
    }

    #[tokio::test]
    async fn writes_only_the_scratch() {
        let Some(h) = docker("write") else { return };
        let r = run(
            "for p in /work/x /work/source.txt /etc/x /x /tmp/x /dev/x /dev/shm/x; do \
               (echo w >> $p) 2>/dev/null && echo wrote $p; done\n\
             echo ok > /scratch/ok && echo scratch",
            CEILING,
            &[],
        );
        let v = result(&h, &r).await;
        assert_eq!(v["exit"], 0);
        assert_eq!(output(&v), ["scratch"]);
        assert_eq!(
            std::fs::read_to_string(h.cwd.join("source.txt")).unwrap(),
            "source"
        );
        assert!(!h.cwd.join("x").exists());
    }

    #[tokio::test]
    async fn holds_memory_processes_file_size_and_time() {
        let Some(h) = docker("limits") else { return };
        let small = Limits {
            memory_mb: 64,
            ..CEILING
        };
        let v = result(
            &h,
            &run("x=$(yes | head -c 268435456); echo survived", small, &[]),
        )
        .await;
        assert_ne!(v["exit"], 0, "{v}");
        assert!(!output(&v).contains(&"survived".into()));

        let few = Limits {
            pids: 16,
            ..CEILING
        };
        let v = result(
            &h,
            &run(
                // A failed fork ends the shell that tried it, so the subshell tries.
                "(for i in $(seq 64); do sleep 30 >/dev/null 2>&1 & done) 2>/dev/null\n\
                 read max < /sys/fs/cgroup/pids.max; read now < /sys/fs/cgroup/pids.current\n\
                 echo \"pids $max $now\"",
                few,
                &[],
            ),
        )
        .await;
        let line = output(&v)
            .into_iter()
            .find(|l| l.starts_with("pids "))
            .unwrap_or_else(|| panic!("no pids line in {v}"));
        let [_, max, now] = line.split(' ').collect::<Vec<_>>()[..] else {
            panic!("{line}")
        };
        assert_eq!(max, "16");
        assert!(now.parse::<u32>().unwrap() <= 16, "{line}");

        let tiny = Limits {
            file_size_mb: 1,
            ..CEILING
        };
        let v = result(
            &h,
            &run(
                "head -c 2097152 /dev/zero > /scratch/f; wc -c < /scratch/f",
                tiny,
                &[],
            ),
        )
        .await;
        assert_eq!(output(&v).last().map(|l| l.trim()), Some("1048576"));

        let short = Limits {
            timeout_secs: 2,
            ..CEILING
        };
        let v = result(&h, &run("sleep 60", short, &["f"])).await;
        assert_eq!(
            (v["timed_out"].as_bool(), v["exit"].is_null()),
            (Some(true), true)
        );
        assert!(v["skipped"]["f"].is_string());
    }

    #[tokio::test]
    async fn sees_only_the_allowed_environment() {
        let Some(h) = docker("env") else { return };
        let lines = output(&result(&h, &run("env", CEILING, &[])).await);
        assert!(lines.contains(&"HOME=/scratch/home".into()), "{lines:?}");
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("SESSION_SECRET") || l.contains("DOCKER")),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn promotes_only_listed_files_within_the_cap() {
        let Some(h) = docker("art") else { return };
        let r = run(
            "echo hi > /scratch/a; echo no > /scratch/b; mkfifo /scratch/fifo\n\
             head -c 17000000 /dev/zero > /scratch/big",
            CEILING,
            &["a", "big", "missing", "fifo"],
        );
        let v = result(&h, &r).await;
        let promoted = h.runs.join("r1.artifact.a");
        assert_eq!(v["artifacts"], json!({"a": promoted.display().to_string()}));
        assert_eq!(std::fs::read_to_string(promoted).unwrap(), "hi\n");
        for skipped in ["big", "missing", "fifo"] {
            assert!(v["skipped"][skipped].is_string(), "{skipped} in {v}");
            assert!(!h.runs.join(format!("r1.artifact.{skipped}")).exists());
        }
        assert!(v["skipped"]["big"].as_str().unwrap().contains("larger"));
        assert!(!h.runs.join("r1.artifact.b").exists());
    }

    #[tokio::test]
    async fn answers_an_error_without_the_image() {
        let Some(mut h) = docker("noimg") else { return };
        h.sandbox_image = Some("mothership-no-such-image:none".into());
        let e = h
            .sandbox("r1", &run("true", CEILING, &[]))
            .await
            .unwrap_err();
        assert!(e.contains("cannot start a container"), "{e}");
    }
}
