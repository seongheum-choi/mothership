//! Where one field of a turn's choice comes from: each scope's candidate, the narrowest one
//! that is set, and the line that names the values and their sources.

use super::labels;
use crate::config::ModelFields;

/// Where one field's value came from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Source<'a> {
    Message,
    Label(&'a str),
    Mode(&'a str),
    Repo(&'a str),
    Instance,
}

impl std::fmt::Display for Source<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message => f.write_str("from your message"),
            Self::Label(label) => write!(f, "label `{label}`"),
            Self::Mode(name) => write!(f, "mode `{name}`"),
            Self::Repo(name) => write!(f, "repo `{name}`"),
            Self::Instance => f.write_str("instance default"),
        }
    }
}

/// One field's value and where it came from, or `None` when that scope leaves it open.
pub(super) type Candidate<'a, T = String> = Option<(T, Source<'a>)>;

/// What a session's message set.
pub(super) fn from_message<'a, T: Clone>(value: Option<&T>) -> Candidate<'a, T> {
    Some((value?.clone(), Source::Message))
}

/// What a `<prefix>:<value>` label set, read with `read`; selection checked it, so it reads.
pub(super) fn from_label<T>(
    label: Option<&String>,
    read: fn(&str) -> Option<T>,
) -> Candidate<'_, T> {
    let label = label?;
    Some((read(label)?, Source::Label(label)))
}

/// The value of a `model:`/`effort:` label.
pub(super) fn label_value(label: &str) -> Option<String> {
    labels::value(label).map(String::from)
}

/// What a mode or a repository set for one field.
pub(super) fn from_scope<'a, T: Clone>(
    scope: Option<(&'a str, &'a ModelFields)>,
    source: fn(&'a str) -> Source<'a>,
    field: impl Fn(&ModelFields) -> Option<&T>,
) -> Candidate<'a, T> {
    let (name, fields) = scope?;
    Some((field(fields)?.clone(), source(name)))
}

/// The narrowest candidate that is set, with the instance default behind them all.
pub(super) fn pick<T>(narrow_first: [Candidate<'_, T>; 4], instance: T) -> (T, Source<'_>) {
    narrow_first
        .into_iter()
        .flatten()
        .next()
        .unwrap_or((instance, Source::Instance))
}

/// `pick` for the effort, which the instance may leave unset so Claude Code decides.
pub(super) fn pick_optional<'a>(
    narrow_first: [Candidate<'a>; 4],
    instance: Option<&String>,
) -> (Option<String>, Source<'a>) {
    match narrow_first.into_iter().flatten().next() {
        Some((value, from)) => (Some(value), from),
        None => (instance.cloned(), Source::Instance),
    }
}

/// One line for the fields, capitalised, with the source once at the end when they share it.
pub(super) fn say(fields: &[(String, Source<'_>)]) -> String {
    let (_, first) = &fields[0];
    let line = if fields.iter().all(|(_, from)| from == first) {
        let values: Vec<&str> = fields.iter().map(|(text, _)| text.as_str()).collect();
        format!("{} ({first})", values.join(", "))
    } else {
        fields
            .iter()
            .map(|(text, from)| format!("{text} ({from})"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut chars = line.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}
