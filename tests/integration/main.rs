//! Integration tests: the mothership binary against local stand-ins for Linear, Zulip, GitHub
//! and the Claude Code CLI. The harness is our own, so this executable can also play `claude`
//! and `gh` when it is started under those names.

mod fake;
mod harness;
mod mock;
mod scenarios;

use scenarios::Scenario;
use std::{
    path::Path,
    process::ExitCode,
    time::{Duration, Instant},
};

/// Longer than any scenario should take, so a hang fails instead of stalling CI.
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    let argv0 = std::env::args().next().unwrap_or_default();
    match Path::new(&argv0).file_name().and_then(|n| n.to_str()) {
        Some("claude") => return fake::claude(),
        Some("gh") => return fake::gh(),
        _ => {}
    }
    let args = Args::parse(std::env::args().skip(1));
    if args.list {
        for (name, _) in selected(args.filter.as_deref()) {
            println!("{name}: test");
        }
        return ExitCode::SUCCESS;
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime can be built")
        .block_on(run(args.filter.as_deref()))
}

/// The libtest arguments this harness understands: a name filter and `--list`. Other flags are
/// ignored, along with the value of those that take one, so `--format terse` is no filter.
#[derive(Debug, Default, PartialEq)]
struct Args {
    filter: Option<String>,
    list: bool,
}

impl Args {
    fn parse(args: impl IntoIterator<Item = String>) -> Self {
        const TAKE_VALUE: &[&str] = &[
            "--color",
            "--format",
            "--logfile",
            "--skip",
            "--test-threads",
            "-Z",
        ];
        let mut parsed = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if arg == "--list" {
                parsed.list = true;
            } else if TAKE_VALUE.contains(&arg.as_str()) {
                args.next();
            } else if !arg.starts_with('-') && parsed.filter.is_none() {
                parsed.filter = Some(arg);
            }
        }
        parsed
    }
}

/// The scenarios whose name contains `filter`, in order.
fn selected(filter: Option<&str>) -> impl Iterator<Item = &'static (&'static str, Scenario)> {
    scenarios::ALL
        .iter()
        .filter(move |(name, _)| filter.is_none_or(|f| name.contains(f)))
}

/// Runs the selected scenarios side by side, each with its own mothership, and reports them
/// in order. A failed scenario keeps its directory and shows the end of mothership's log.
async fn run(filter: Option<&str>) -> ExitCode {
    let started = Instant::now();
    let selected: Vec<_> = selected(filter).collect();
    println!("\nrunning {} scenarios", selected.len());
    let running: Vec<_> = selected
        .into_iter()
        .map(|&(name, scenario)| {
            let ctx = harness::Ctx::default();
            let task = tokio::spawn(tokio::time::timeout(
                SCENARIO_TIMEOUT,
                scenario(ctx.clone()),
            ));
            (name, ctx, task)
        })
        .collect();
    let (mut passed, mut failed) = (0, 0);
    for (name, ctx, task) in running {
        let result = match task.await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(anyhow::anyhow!("timed out after {SCENARIO_TIMEOUT:?}")),
            Err(e) => Err(anyhow::anyhow!("panicked: {e}")),
        };
        match result {
            Ok(()) => {
                passed += 1;
                println!("scenario {name} ... ok");
                ctx.clean_up();
            }
            Err(e) => {
                failed += 1;
                println!("scenario {name} ... FAILED\n  {e:#}");
                ctx.report();
            }
        }
    }
    println!(
        "\nintegration: {passed} passed; {failed} failed; finished in {:.1}s\n",
        started.elapsed().as_secs_f64()
    );
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
