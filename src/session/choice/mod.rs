//! A turn's model, fallback model and effort, each decided on its own by the narrowest scope
//! that sets it: `[model=…]`/`[effort=…]` in the session's messages, then the issue's
//! `model:`/`effort:` labels, then the mode its labels picked, then its repository, then the
//! instance defaults. A change since the last turn is said in the turn's first thought.

mod directives;
pub mod labels;

pub use directives::record;

use super::Outcome;
use crate::{
    agent::Models,
    app::App,
    config::{ModelDefaults, ModelFields},
    store::{Launched, SessionRec},
};

/// The scopes between the session's own fields and the instance defaults, which `choose` reads
/// from the store and `.env` itself. A chat session has neither.
#[derive(Default)]
pub struct Scopes<'a> {
    /// The mode's name and the fields its frontmatter sets.
    pub mode: Option<(&'a str, &'a ModelFields)>,
    /// The repository's name and the fields `repos.json` sets for it.
    pub repo: Option<(&'a str, &'a ModelFields)>,
}

/// Where one field's value came from.
#[derive(Clone, Copy, PartialEq)]
enum Source<'a> {
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
type Candidate<'a> = Option<(String, Source<'a>)>;

/// What a session's message set.
fn from_message<'a>(value: Option<&String>) -> Candidate<'a> {
    Some((value?.clone(), Source::Message))
}

/// What a `model:`/`effort:` label set; selection checked it, so its value parses.
fn from_label(label: Option<&String>) -> Candidate<'_> {
    let label = label?;
    Some((labels::value(label)?.to_string(), Source::Label(label)))
}

/// What a mode or a repository set for one field.
fn from_scope<'a>(
    scope: Option<(&'a str, &'a ModelFields)>,
    source: fn(&'a str) -> Source<'a>,
    field: fn(&ModelFields) -> Option<&String>,
) -> Candidate<'a> {
    let (name, fields) = scope?;
    Some((field(fields)?.clone(), source(name)))
}

/// The narrowest candidate that is set, with the instance default behind them all.
fn pick<'a>(narrow_first: [Candidate<'a>; 4], instance: &str) -> (String, Source<'a>) {
    narrow_first
        .into_iter()
        .flatten()
        .next()
        .unwrap_or_else(|| (instance.to_string(), Source::Instance))
}

/// `pick` for the effort, which the instance may leave unset so Claude Code decides.
fn pick_optional<'a>(
    narrow_first: [Candidate<'a>; 4],
    instance: Option<&String>,
) -> (Option<String>, Source<'a>) {
    match narrow_first.into_iter().flatten().next() {
        Some((value, from)) => (Some(value), from),
        None => (instance.cloned(), Source::Instance),
    }
}

/// The models for the next turn of `key` and, when they changed since its last turn (or
/// anything but the instance defaults decided them on its first), the line that says so.
pub fn for_turn(
    app: &App,
    key: &str,
    rec: &SessionRec,
    scopes: &Scopes<'_>,
) -> (Models, Option<String>) {
    let defaults = app.models();
    let (models, note) = choose(rec, scopes, &defaults);
    let launched = Launched {
        model: models.model.clone(),
        fallback: models.fallback.clone(),
        effort: models.effort.clone(),
    };
    app.store.update(|s| {
        if let Some(rec) = s.sessions.get_mut(key) {
            rec.launched = Some(launched);
        }
    });
    (models, note)
}

fn choose<'a>(
    rec: &'a SessionRec,
    scopes: &Scopes<'a>,
    defaults: &ModelDefaults,
) -> (Models, Option<String>) {
    let mode =
        |field: fn(&ModelFields) -> Option<&String>| from_scope(scopes.mode, Source::Mode, field);
    let repo =
        |field: fn(&ModelFields) -> Option<&String>| from_scope(scopes.repo, Source::Repo, field);
    let (model, model_from) = pick(
        [
            from_message(rec.model.as_ref()),
            from_label(rec.label_model.as_ref()),
            mode(|f| f.model.as_ref()),
            repo(|f| f.model.as_ref()),
        ],
        &defaults.model,
    );
    // No message or label sets the fallback model; a mode or a repository does.
    let (fallback, fallback_from) = pick(
        [
            None,
            None,
            mode(|f| f.fallback_model.as_ref()),
            repo(|f| f.fallback_model.as_ref()),
        ],
        &defaults.fallback_model,
    );
    let (effort, effort_from) = pick_optional(
        [
            from_message(rec.effort.as_ref()),
            from_label(rec.label_effort.as_ref()),
            mode(|f| f.effort.as_ref()),
            repo(|f| f.effort.as_ref()),
        ],
        defaults.effort.as_ref(),
    );
    let unchanged = match &rec.launched {
        Some(last) => last.model == model && last.fallback == fallback && last.effort == effort,
        None => [model_from, fallback_from, effort_from]
            .iter()
            .all(|from| *from == Source::Instance),
    };
    let note = (!unchanged).then(|| {
        let level = effort.as_deref().unwrap_or("default");
        let mut fields = vec![(format!("Model: {model}"), model_from)];
        // The fallback model is only worth a line when something other than the instance set it.
        if fallback_from != Source::Instance {
            fields.push((format!("fallback: {fallback}"), fallback_from));
        }
        fields.push((format!("effort: {level}"), effort_from));
        say(&fields)
    });
    let models = Models {
        model,
        fallback,
        effort,
    };
    (models, note)
}

/// One line for the fields, with the source once at the end when they share it.
fn say(fields: &[(String, Source<'_>)]) -> String {
    let (_, first) = &fields[0];
    let values = |sep: &str| {
        fields
            .iter()
            .map(|(text, _)| text.clone())
            .collect::<Vec<_>>()
            .join(sep)
    };
    if fields.iter().all(|(_, from)| from == first) {
        return format!("{} ({first})", values(", "));
    }
    fields
        .iter()
        .map(|(text, from)| format!("{text} ({from})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// After a turn: a model a message chose stays once a turn with it answered, and gives way to
/// the one before it when that turn failed, so an unknown model does not fail every turn.
pub fn settle(app: &App, key: &str, outcome: &Outcome) {
    let failed = match outcome {
        Outcome::Reply(_) => false,
        Outcome::Failed(_) => true,
        Outcome::Stopped(_) => return,
    };
    app.store.update(|s| {
        let Some(rec) = s.sessions.get_mut(key) else {
            return;
        };
        let tried = matches!((&rec.launched, &rec.model), (Some(l), Some(m)) if l.model == *m);
        if let Some(replaced) = rec.replaced_model.take_if(|_| tried)
            && failed
        {
            tracing::warn!(
                "[{key}] the turn on model {:?} failed; going back",
                rec.model
            );
            rec.model = replaced.model;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults(effort: Option<&str>) -> ModelDefaults {
        ModelDefaults {
            model: "opus".into(),
            fallback_model: "sonnet".into(),
            effort: effort.map(Into::into),
        }
    }

    fn fields(model: Option<&str>, fallback: Option<&str>, effort: Option<&str>) -> ModelFields {
        ModelFields {
            model: model.map(Into::into),
            fallback_model: fallback.map(Into::into),
            effort: effort.map(Into::into),
        }
    }

    #[test]
    fn message_beats_mode_beats_repo_beats_instance_per_field() {
        let mut rec = SessionRec::default();
        let (models, note) = choose(&rec, &Scopes::default(), &defaults(None));
        assert_eq!(
            (
                models.model.as_str(),
                models.fallback.as_str(),
                models.effort
            ),
            ("opus", "sonnet", None)
        );
        assert_eq!(note, None, "nothing but the instance decided anything");

        // The repository sets the model; the mode leaves it and sets the effort.
        let repo = fields(Some("haiku"), None, Some("low"));
        let mode = fields(None, Some("haiku"), Some("high"));
        let scopes = || Scopes {
            mode: Some(("research", &mode)),
            repo: Some(("vault", &repo)),
        };
        let (models, note) = choose(&rec, &scopes(), &defaults(None));
        assert_eq!(
            (models.model.as_str(), models.fallback.as_str()),
            ("haiku", "haiku")
        );
        assert_eq!(models.effort.as_deref(), Some("high"));
        assert_eq!(
            note.as_deref(),
            Some(
                "Model: haiku (repo `vault`), fallback: haiku (mode `research`), \
                 effort: high (mode `research`)"
            )
        );

        rec.model = Some("opus".into());
        rec.effort = Some("max".into());
        let (models, note) = choose(&rec, &scopes(), &defaults(None));
        assert_eq!(models.model.as_str(), "opus");
        assert_eq!(models.effort.as_deref(), Some("max"));
        assert_eq!(
            note.as_deref(),
            Some(
                "Model: opus (from your message), fallback: haiku (mode `research`), \
                 effort: max (from your message)"
            )
        );
    }

    #[test]
    fn a_label_beats_the_mode_and_gives_way_to_a_message() {
        let mode = fields(Some("haiku"), None, Some("low"));
        let scopes = || Scopes {
            mode: Some(("research", &mode)),
            repo: None,
        };
        let mut rec = SessionRec {
            label_model: Some("Model:sonnet".into()),
            label_effort: Some("effort: high".into()),
            ..SessionRec::default()
        };
        let (models, note) = choose(&rec, &scopes(), &defaults(None));
        assert_eq!(models.model.as_str(), "sonnet");
        assert_eq!(models.effort.as_deref(), Some("high"));
        assert_eq!(
            note.as_deref(),
            Some("Model: sonnet (label `Model:sonnet`), effort: high (label `effort: high`)")
        );

        rec.model = Some("opus".into());
        let (models, _) = choose(&rec, &scopes(), &defaults(None));
        assert_eq!(models.model.as_str(), "opus", "a message beats the label");
    }

    #[test]
    fn one_source_is_said_once() {
        let rec = SessionRec {
            model: Some("sonnet".into()),
            effort: Some("high".into()),
            ..SessionRec::default()
        };
        let (_, note) = choose(&rec, &Scopes::default(), &defaults(None));
        assert_eq!(
            note.as_deref(),
            Some("Model: sonnet, effort: high (from your message)")
        );
    }

    #[test]
    fn says_only_what_changed() {
        let mut rec = SessionRec {
            launched: Some(Launched {
                model: "opus".into(),
                fallback: "sonnet".into(),
                effort: None,
            }),
            ..SessionRec::default()
        };
        assert_eq!(choose(&rec, &Scopes::default(), &defaults(None)).1, None);
        let (_, note) = choose(&rec, &Scopes::default(), &defaults(Some("high")));
        assert_eq!(
            note.as_deref(),
            Some("Model: opus, effort: high (instance default)")
        );
        rec.launched = None;
        assert_eq!(
            choose(&rec, &Scopes::default(), &defaults(Some("high"))).1,
            None,
            "a first turn on the instance defaults says nothing"
        );
    }
}
