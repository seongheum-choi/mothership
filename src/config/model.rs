//! The runner, model, fallback model and effort turns run with unless a session says otherwise:
//! `AGENT_RUNNER`, `CLAUDE_MODEL`, `CLAUDE_FALLBACK_MODEL` and `CLAUDE_EFFORT`, re-read whenever
//! `.env` changes.

use super::vars::Vars;
use crate::agent::Runner;

/// The effort levels `runner` takes: Claude Code's `--effort` (2.1.331) and Codex's
/// `model_reasoning_effort` (`supported_reasoning_levels` of Codex 0.172's models).
pub const fn efforts(runner: Runner) -> &'static [&'static str] {
    match runner {
        Runner::Claude => &["low", "medium", "high", "xhigh", "max"],
        Runner::Codex => &["low", "medium", "high", "xhigh", "max", "ultra"],
    }
}

/// The effort levels a scope may set: every level some runner takes, which is Codex's list.
/// One the turn's runner does not take is left off that turn (see `session::choice`).
pub const EFFORTS: &[&str] = efforts(Runner::Codex);

/// What one scope sets: a mode file, a repository in `repos.json`, or an issue's labels. A
/// `None` field leaves that field to a wider scope, down to the instance defaults.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelFields {
    pub runner: Option<Runner>,
    pub model: Option<String>,
    pub fallback_model: Option<String>,
    /// One of `EFFORTS`, checked where the scope is read so a bad value is named there, and
    /// against the turn's runner when the turn starts.
    pub effort: Option<String>,
}

pub fn check_effort(effort: &str) -> Result<(), String> {
    if EFFORTS.contains(&effort) {
        return Ok(());
    }
    Err(format!(
        "`{effort}` is not an effort level; use one of {}",
        EFFORTS.join(", ")
    ))
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelDefaults {
    pub runner: Runner,
    pub model: String,
    pub fallback_model: String,
    /// `None` leaves the effort off, so the runner picks.
    pub effort: Option<String>,
}

impl ModelDefaults {
    pub(super) fn from_vars(vars: &Vars) -> Self {
        let effort = vars.get("CLAUDE_EFFORT").and_then(|effort| {
            if EFFORTS.contains(&effort.as_str()) {
                return Some(effort);
            }
            tracing::warn!(
                "settings: CLAUDE_EFFORT={effort} is not one of {}; leaving the effort to the runner",
                EFFORTS.join(", ")
            );
            None
        });
        Self {
            runner: runner(vars),
            model: vars.get("CLAUDE_MODEL").unwrap_or_else(|| "opus".into()),
            fallback_model: vars
                .get("CLAUDE_FALLBACK_MODEL")
                .unwrap_or_else(|| "sonnet".into()),
            effort,
        }
    }
}

/// `AGENT_RUNNER`; anything but `claude` or `codex` is warned about and runs Claude Code.
fn runner(vars: &Vars) -> Runner {
    let Some(value) = vars.get("AGENT_RUNNER") else {
        return Runner::default();
    };
    value.parse().unwrap_or_else(|e| {
        tracing::warn!("settings: AGENT_RUNNER: {e}; running claude");
        Runner::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_takes_every_effort_claude_code_does() {
        let (claude, codex) = (efforts(Runner::Claude), efforts(Runner::Codex));
        assert!(
            claude.iter().all(|e| codex.contains(e)),
            "so EFFORTS is Codex's"
        );
        assert!(codex.contains(&"ultra") && !claude.contains(&"ultra"));
        assert_eq!(check_effort("ultra"), Ok(()));
        assert!(check_effort("minimal").is_err());
    }
}
