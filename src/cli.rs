//! The command line: running the service, or asking the instance already running in
//! `MOTHERSHIP_HOME` about Linear agent sessions through its control socket.

use crate::control::{Answer, Request};
use anyhow::{Context, Result, bail};
use std::{
    io::{IsTerminal, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    process::ExitCode,
    time::Duration,
};

const USAGE: &str = "\
Usage:
  mothership                                     run the service
  mothership session status <issue>              each agent session of the issue: its Linear
                                                 status, its turn, its last report
  mothership session reply [--dry-run] <issue>   reply, read from stdin, in the thread of the
                                                 issue's newest session; its next turn starts.
                                                 --dry-run only names the session and comment
  mothership --help
";

/// How long the instance may take to answer: reading every activity of every session.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, PartialEq)]
pub enum Command {
    Serve,
    Help,
    Session(Session),
}

/// What `mothership session` asks the running instance.
#[derive(Debug, PartialEq)]
pub enum Session {
    Status { issue: String },
    Reply { issue: String, dry_run: bool },
}

impl Command {
    /// Reads the arguments after the program name. `None` for anything else.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Option<Self> {
        let args: Vec<String> = args.into_iter().collect();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        if args.iter().any(|a| matches!(*a, "-h" | "--help")) {
            return Some(Self::Help);
        }
        let issue = |s: &str| (!s.starts_with('-')).then(|| s.to_string());
        Some(match args.as_slice() {
            [] => Self::Serve,
            ["help"] => Self::Help,
            ["session", "status", key] => Self::Session(Session::Status { issue: issue(key)? }),
            ["session", "reply", key] => Self::Session(Session::Reply {
                issue: issue(key)?,
                dry_run: false,
            }),
            ["session", "reply", "--dry-run", key] | ["session", "reply", key, "--dry-run"] => {
                Self::Session(Session::Reply {
                    issue: issue(key)?,
                    dry_run: true,
                })
            }
            _ => return None,
        })
    }
}

/// Prints the usage to stdout, or to stderr after arguments it does not take.
pub fn usage(asked: bool) -> ExitCode {
    if asked {
        let _ = std::io::stdout().write_all(USAGE.as_bytes());
        ExitCode::SUCCESS
    } else {
        let _ = std::io::stderr().write_all(USAGE.as_bytes());
        ExitCode::from(2)
    }
}

pub fn session(command: Session) -> Result<ExitCode> {
    let request = match command {
        Session::Status { issue } => Request::Status { issue },
        Session::Reply { issue, dry_run } => {
            let mut stdin = std::io::stdin();
            if stdin.is_terminal() {
                bail!("pipe the reply on stdin: mothership session reply {issue} < reply.md");
            }
            let mut body = String::new();
            stdin
                .read_to_string(&mut body)
                .context("reading the reply")?;
            Request::Reply {
                issue,
                body,
                dry_run,
            }
        }
    };
    let text = ask(&request)?;
    std::io::stdout().write_all(text.as_bytes())?;
    Ok(ExitCode::SUCCESS)
}

fn ask(request: &Request) -> Result<String> {
    let path = crate::control::socket(&crate::config::home()?);
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("no running mothership answers on {}", path.display()))?;
    stream.set_read_timeout(Some(ANSWER_TIMEOUT))?;
    serde_json::to_writer(&mut stream, request)?;
    stream.shutdown(Shutdown::Write)?;
    let mut answer = String::new();
    stream
        .read_to_string(&mut answer)
        .context("reading the instance's answer")?;
    let answer: Answer =
        serde_json::from_str(&answer).context("the instance's answer is not JSON")?;
    if !answer.ok {
        bail!("{}", answer.text);
    }
    Ok(answer.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Option<Command> {
        Command::parse(args.iter().map(ToString::to_string))
    }

    #[test]
    fn runs_the_service_without_arguments() {
        assert_eq!(parse(&[]), Some(Command::Serve));
    }

    #[test]
    fn help_wins_anywhere() {
        for args in [&["--help"][..], &["-h"], &["help"], &["session", "--help"]] {
            assert_eq!(parse(args), Some(Command::Help), "{args:?}");
        }
    }

    #[test]
    fn reads_the_session_commands() {
        assert_eq!(
            parse(&["session", "status", "SH-1"]),
            Some(Command::Session(Session::Status {
                issue: "SH-1".into()
            }))
        );
        let reply = |dry_run| {
            Some(Command::Session(Session::Reply {
                issue: "SH-1".into(),
                dry_run,
            }))
        };
        assert_eq!(parse(&["session", "reply", "SH-1"]), reply(false));
        assert_eq!(
            parse(&["session", "reply", "--dry-run", "SH-1"]),
            reply(true)
        );
        assert_eq!(
            parse(&["session", "reply", "SH-1", "--dry-run"]),
            reply(true)
        );
    }

    #[test]
    fn refuses_what_it_does_not_know() {
        for args in [
            &["serve", "now"][..],
            &["--port", "1"],
            &["session"],
            &["session", "status"],
            &["session", "status", "SH-1", "SH-2"],
            &["session", "reply", "--force", "SH-1"],
            &["session", "status", "--dry-run"],
        ] {
            assert_eq!(parse(args), None, "{args:?}");
        }
    }
}
