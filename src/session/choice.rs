//! A turn's model and effort, field by field: `[model=…]`/`[effort=…]` from the session's
//! messages, then the mode's model, then the instance defaults. A change since the last turn is
//! said in the turn's first thought.

use super::Outcome;
use crate::{
    agent::Models,
    app::App,
    config::{EFFORTS, ModelDefaults},
    directive,
    store::{Launched, Replaced, SessionRec},
};

/// What one message's directive does to a field.
#[derive(Debug, Default, PartialEq)]
enum Set {
    #[default]
    Keep,
    /// `default`: back to the mode and the instance.
    Clear,
    To(String),
}

impl Set {
    fn apply(self, field: &mut Option<String>) {
        match self {
            Self::Keep => {}
            Self::Clear => *field = None,
            Self::To(value) => *field = Some(value),
        }
    }
}

#[derive(Debug, Default, PartialEq)]
struct Directives {
    model: Set,
    effort: Set,
}

/// The first `[key=…]` of `text` that is not a placeholder such as `[model=<name>]`.
fn value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    directive::values(text, key).find(|v| !v.contains(['<', '>', '…']))
}

fn parse(text: &str) -> Result<Directives, String> {
    let model = match value(text, "model") {
        Some("") => return Err("`[model=]` needs a model name, or `default`.".into()),
        Some("default") => Set::Clear,
        Some(model) => Set::To(model.to_string()),
        None => Set::Keep,
    };
    let effort = match value(text, "effort") {
        Some("default") => Set::Clear,
        Some(effort) if EFFORTS.contains(&effort) => Set::To(effort.to_string()),
        Some(effort) => {
            return Err(format!(
                "`[effort={effort}]` is not an effort level; use one of {}, or `default`.",
                EFFORTS.join(", ")
            ));
        }
        None => Set::Keep,
    };
    Ok(Directives { model, effort })
}

/// Records the directives in `text` on the session, for the turns that start after it. A bad
/// one changes nothing and comes back as the problem to report.
pub fn record(app: &App, key: &str, text: &str) -> Result<(), String> {
    let directives = parse(text)?;
    if directives == Directives::default() {
        return Ok(());
    }
    tracing::info!("[{key}] directives: {directives:?}");
    app.store.update(|s| {
        let rec = s.sessions.entry(key.to_string()).or_default();
        if let Set::To(model) = &directives.model
            && rec.model.as_ref() != Some(model)
            && rec.replaced_model.is_none()
        {
            rec.replaced_model = Some(Replaced {
                model: rec.model.clone(),
            });
        }
        directives.model.apply(&mut rec.model);
        directives.effort.apply(&mut rec.effort);
    });
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum Source<'a> {
    Message,
    Mode(&'a str),
    Instance,
}

impl std::fmt::Display for Source<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message => f.write_str("from your message"),
            Self::Mode(name) => write!(f, "mode `{name}`"),
            Self::Instance => f.write_str("instance default"),
        }
    }
}

/// The models for the next turn of `key` and, when they changed since its last turn (or a
/// message sets them on its first), the line that says so. `mode` is the mode's name and model.
pub fn for_turn(
    app: &App,
    key: &str,
    rec: &SessionRec,
    mode: Option<(&str, String)>,
) -> (Models, Option<String>) {
    let defaults = app.models();
    let (models, note) = choose(rec, mode, &defaults);
    let launched = Launched {
        model: models.model.clone(),
        effort: models.effort.clone(),
    };
    app.store.update(|s| {
        if let Some(rec) = s.sessions.get_mut(key) {
            rec.launched = Some(launched);
        }
    });
    (models, note)
}

fn choose(
    rec: &SessionRec,
    mode: Option<(&str, String)>,
    defaults: &ModelDefaults,
) -> (Models, Option<String>) {
    let (model, model_from) = match (&rec.model, mode) {
        (Some(model), _) => (model.clone(), Source::Message),
        (None, Some((name, model))) => (model, Source::Mode(name)),
        (None, None) => (defaults.model.clone(), Source::Instance),
    };
    let (effort, effort_from) = match &rec.effort {
        Some(effort) => (Some(effort.clone()), Source::Message),
        None => (defaults.effort.clone(), Source::Instance),
    };
    let unchanged = match &rec.launched {
        Some(last) => last.model == model && last.effort == effort,
        None => model_from != Source::Message && effort_from != Source::Message,
    };
    let note = (!unchanged).then(|| {
        let level = effort.as_deref().unwrap_or("default");
        if model_from == effort_from {
            format!("Model: {model}, effort: {level} ({model_from})")
        } else {
            format!("Model: {model} ({model_from}), effort: {level} ({effort_from})")
        }
    });
    let models = Models {
        model,
        fallback: defaults.fallback_model.clone(),
        effort,
    };
    (models, note)
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

    #[test]
    fn parses_directives() {
        let both = parse("[model=sonnet] please [effort= high ]").unwrap();
        assert_eq!(both.model, Set::To("sonnet".into()));
        assert_eq!(both.effort, Set::To("high".into()));
        let reset = parse("[model=default][effort=default]").unwrap();
        assert_eq!((reset.model, reset.effort), (Set::Clear, Set::Clear));
        assert_eq!(
            parse("say `[effort=<level>]`").unwrap(),
            Directives::default()
        );
        assert!(
            parse("[effort=ultra]")
                .unwrap_err()
                .contains("not an effort level")
        );
        assert!(parse("[model=]").is_err());
    }

    #[test]
    fn message_beats_mode_beats_instance_per_field() {
        let mut rec = SessionRec::default();
        let (models, note) = choose(&rec, None, &defaults(None));
        assert_eq!(
            (models.model.as_str(), models.effort, note),
            ("opus", None, None)
        );

        let mode = || Some(("research", "haiku".to_string()));
        let (models, note) = choose(&rec, mode(), &defaults(Some("low")));
        assert_eq!((models.model.as_str(), note), ("haiku", None));
        assert_eq!(models.effort.as_deref(), Some("low"));

        rec.effort = Some("max".into());
        let (models, note) = choose(&rec, mode(), &defaults(Some("low")));
        assert_eq!(models.effort.as_deref(), Some("max"));
        assert_eq!(
            note.as_deref(),
            Some("Model: haiku (mode `research`), effort: max (from your message)")
        );
        rec.model = Some("sonnet".into());
        let (models, note) = choose(&rec, mode(), &defaults(None));
        assert_eq!(
            (models.model.as_str(), models.fallback.as_str()),
            ("sonnet", "sonnet")
        );
        assert_eq!(
            note.as_deref(),
            Some("Model: sonnet, effort: max (from your message)")
        );
    }

    #[test]
    fn says_only_what_changed() {
        let mut rec = SessionRec {
            launched: Some(Launched {
                model: "opus".into(),
                effort: None,
            }),
            ..SessionRec::default()
        };
        assert_eq!(choose(&rec, None, &defaults(None)).1, None);
        let (_, note) = choose(&rec, None, &defaults(Some("high")));
        assert_eq!(
            note.as_deref(),
            Some("Model: opus, effort: high (instance default)")
        );
        rec.launched = None;
        assert_eq!(choose(&rec, None, &defaults(Some("high"))).1, None);
    }
}
