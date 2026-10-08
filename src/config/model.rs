//! The model, fallback model and effort turns run with unless a session says otherwise:
//! `CLAUDE_MODEL`, `CLAUDE_FALLBACK_MODEL` and `CLAUDE_EFFORT`, re-read whenever `.env` changes.

use super::vars::Vars;

/// The levels Claude Code's `--effort` takes.
pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

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
