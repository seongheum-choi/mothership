//! The model, fallback model and effort turns run with unless a session says otherwise:
//! `CLAUDE_MODEL`, `CLAUDE_FALLBACK_MODEL` and `CLAUDE_EFFORT`, re-read whenever `.env` changes.

use super::vars::Vars;

/// The levels Claude Code's `--effort` takes.
pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// What one scope sets: a mode file, a repository in `repos.json`, or an issue's labels. A
/// `None` field leaves that field to a wider scope, down to the instance defaults.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelFields {
    pub model: Option<String>,
    pub fallback_model: Option<String>,
    /// One of `EFFORTS`, checked where the scope is read so a bad value is named there.
    pub effort: Option<String>,
}

/// The problem to report when `effort` is not one of `EFFORTS`.
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
    pub model: String,
    pub fallback_model: String,
    /// `None` leaves `--effort` off, so Claude Code picks.
    pub effort: Option<String>,
}

impl ModelDefaults {
    pub(super) fn from_vars(vars: &Vars) -> Self {
        let effort = vars.get("CLAUDE_EFFORT").and_then(|effort| {
            if EFFORTS.contains(&effort.as_str()) {
                return Some(effort);
            }
            tracing::warn!(
                "settings: CLAUDE_EFFORT={effort} is not one of {}; leaving the effort to Claude Code",
                EFFORTS.join(", ")
            );
            None
        });
        Self {
            model: vars.get("CLAUDE_MODEL").unwrap_or_else(|| "opus".into()),
            fallback_model: vars
                .get("CLAUDE_FALLBACK_MODEL")
                .unwrap_or_else(|| "sonnet".into()),
            effort,
        }
    }
}
