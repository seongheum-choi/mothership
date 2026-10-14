//! A turn's runner, model, fallback model and effort, each decided on its own by the narrowest
//! scope that sets it: `[runner=…]`/`[model=…]`/`[effort=…]` in the session's messages, then
//! the issue's `runner:`/`model:`/`effort:` labels, then the mode its labels picked, then its
//! repository, then the instance defaults. A change since the last turn is said in the turn's
//! first thought. Claude Code's usage limit moves a turn on Claude to Codex unless a message
//! chose the runner.

mod directives;
pub mod labels;
mod scope;

pub use directives::record;

use super::Outcome;
use crate::{
    agent::{Models, Runner, limit},
    app::App,
    config::{ModelDefaults, ModelFields},
    store::{Launched, SessionRec},
};
use scope::{Source, from_label, from_message, from_scope, label_value, pick, pick_optional, say};

/// The scopes between the session's own fields and the instance defaults, which `choose` reads
/// from the store and `.env` itself. A chat session has neither.
#[derive(Default)]
pub struct Scopes<'a> {
    /// The mode's name and the fields its frontmatter sets.
    pub mode: Option<(&'a str, &'a ModelFields)>,
    /// The repository's name and the fields `repos.json` sets for it.
    pub repo: Option<(&'a str, &'a ModelFields)>,
}

/// What the next turn runs on and with.
#[derive(Debug)]
pub struct Chosen {
    pub runner: Runner,
    /// A message chose the runner, so Claude Code's usage limit does not move the turn.
    pub pinned: bool,
    pub models: Models,
    /// The turn's first thought: what changed since the last turn, and a usage limit switch.
    pub note: Option<String>,
}

/// What the next turn of `key` runs on and with and, when that changed since its last turn (or
/// anything but the instance defaults decided it on its first), the line that says so.
pub fn for_turn(app: &App, key: &str, rec: &SessionRec, scopes: &Scopes<'_>) -> Chosen {
    let defaults = app.models();
    let mut chosen = choose(rec, scopes, &defaults);
    let launched = Launched {
        runner: Some(chosen.runner),
        model: chosen.models.model.clone(),
        fallback: chosen.models.fallback.clone(),
        effort: chosen.models.effort.clone(),
    };
    app.store.update(|s| {
        if let Some(rec) = s.sessions.get_mut(key) {
            rec.launched = Some(launched);
        }
    });
    if !chosen.pinned
        && let Some(resets_at) = app.claude_limit(chosen.runner)
    {
        chosen.runner = Runner::Codex;
        let limited = limit::still_limited(resets_at);
        chosen.note = Some(match chosen.note.take() {
            Some(note) => format!("{limited}\n\n{note}"),
            None => limited,
        });
    }
    chosen
}

fn choose(rec: &SessionRec, scopes: &Scopes<'_>, defaults: &ModelDefaults) -> Chosen {
    let mode = scopes.mode;
    let repo = scopes.repo;
    let (runner, runner_from) = pick(
        [
            from_message(rec.runner.as_ref()),
            from_label(rec.label_runner.as_ref(), labels::runner),
            from_scope(mode, Source::Mode, |f| f.runner.as_ref()),
            from_scope(repo, Source::Repo, |f| f.runner.as_ref()),
        ],
        defaults.runner,
    );
    let (model, model_from) = pick(
        [
            from_message(rec.model.as_ref()),
            from_label(rec.label_model.as_ref(), label_value),
            from_scope(mode, Source::Mode, |f| f.model.as_ref()),
            from_scope(repo, Source::Repo, |f| f.model.as_ref()),
        ],
        defaults.model.clone(),
    );
    // No message or label sets the fallback model; a mode or a repository does.
    let (fallback, fallback_from) = pick(
        [
            None,
            None,
            from_scope(mode, Source::Mode, |f| f.fallback_model.as_ref()),
            from_scope(repo, Source::Repo, |f| f.fallback_model.as_ref()),
        ],
        defaults.fallback_model.clone(),
    );
    let (effort, effort_from) = pick_optional(
        [
            from_message(rec.effort.as_ref()),
            from_label(rec.label_effort.as_ref(), label_value),
            from_scope(mode, Source::Mode, |f| f.effort.as_ref()),
            from_scope(repo, Source::Repo, |f| f.effort.as_ref()),
        ],
        defaults.effort.as_ref(),
    );
    let last = rec.launched.as_ref();
    // A turn from before runners were recorded ran on whatever runner this one does.
    let runner_changed = last.and_then(|l| l.runner).is_some_and(|r| r != runner);
    let unchanged = match last {
        Some(last) => {
            !runner_changed
                && last.model == model
                && last.fallback == fallback
                && last.effort == effort
        }
        None => [runner_from, model_from, fallback_from, effort_from]
            .iter()
            .all(|from| *from == Source::Instance),
    };
    let note = (!unchanged).then(|| {
        let level = effort.as_deref().unwrap_or("default");
        let mut fields = Vec::new();
        if runner_from != Source::Instance || runner_changed {
            fields.push((format!("runner: {runner}"), runner_from));
        }
        fields.push((format!("model: {model}"), model_from));
        // The fallback model is only worth a line when something other than the instance set it.
        if fallback_from != Source::Instance {
            fields.push((format!("fallback: {fallback}"), fallback_from));
        }
        fields.push((format!("effort: {level}"), effort_from));
        say(&fields)
    });
    Chosen {
        runner,
        pinned: runner_from == Source::Message,
        models: Models {
            model,
            fallback,
            effort,
        },
        note,
    }
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
            runner: Runner::Claude,
            model: "opus".into(),
            fallback_model: "sonnet".into(),
            effort: effort.map(Into::into),
        }
    }

    fn fields(model: Option<&str>, fallback: Option<&str>, effort: Option<&str>) -> ModelFields {
        ModelFields {
            runner: None,
            model: model.map(Into::into),
            fallback_model: fallback.map(Into::into),
            effort: effort.map(Into::into),
        }
    }

    #[test]
    fn message_beats_mode_beats_repo_beats_instance_per_field() {
        let mut rec = SessionRec::default();
        let Chosen { models, note, .. } = choose(&rec, &Scopes::default(), &defaults(None));
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
        let Chosen { models, note, .. } = choose(&rec, &scopes(), &defaults(None));
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
        let Chosen { models, note, .. } = choose(&rec, &scopes(), &defaults(None));
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
        let Chosen { models, note, .. } = choose(&rec, &scopes(), &defaults(None));
        assert_eq!(models.model.as_str(), "sonnet");
        assert_eq!(models.effort.as_deref(), Some("high"));
        assert_eq!(
            note.as_deref(),
            Some("Model: sonnet (label `Model:sonnet`), effort: high (label `effort: high`)")
        );

        rec.model = Some("opus".into());
        let Chosen { models, .. } = choose(&rec, &scopes(), &defaults(None));
        assert_eq!(models.model.as_str(), "opus", "a message beats the label");
    }

    #[test]
    fn one_source_is_said_once() {
        let rec = SessionRec {
            model: Some("sonnet".into()),
            effort: Some("high".into()),
            ..SessionRec::default()
        };
        let Chosen { note, .. } = choose(&rec, &Scopes::default(), &defaults(None));
        assert_eq!(
            note.as_deref(),
            Some("Model: sonnet, effort: high (from your message)")
        );
    }

    #[test]
    fn says_only_what_changed() {
        let mut rec = SessionRec {
            launched: Some(Launched {
                runner: Some(Runner::Claude),
                model: "opus".into(),
                fallback: "sonnet".into(),
                effort: None,
            }),
            ..SessionRec::default()
        };
        assert_eq!(choose(&rec, &Scopes::default(), &defaults(None)).note, None);
        let Chosen { note, .. } = choose(&rec, &Scopes::default(), &defaults(Some("high")));
        assert_eq!(
            note.as_deref(),
            Some("Model: opus, effort: high (instance default)")
        );
        rec.launched = None;
        assert_eq!(
            choose(&rec, &Scopes::default(), &defaults(Some("high"))).note,
            None,
            "a first turn on the instance defaults says nothing"
        );
    }

    #[test]
    fn the_runner_is_decided_like_the_model_and_a_message_pins_it() {
        let mut repo = fields(None, None, None);
        repo.runner = Some(Runner::Codex);
        let scopes = || Scopes {
            mode: None,
            repo: Some(("vault", &repo)),
        };
        let mut rec = SessionRec::default();
        let chosen = choose(&rec, &scopes(), &defaults(None));
        assert_eq!((chosen.runner, chosen.pinned), (Runner::Codex, false));
        assert_eq!(
            chosen.note.as_deref(),
            Some(
                "Runner: codex (repo `vault`), model: opus (instance default), \
                 effort: default (instance default)"
            )
        );

        rec.label_runner = Some("runner:claude".into());
        let chosen = choose(&rec, &scopes(), &defaults(None));
        assert_eq!((chosen.runner, chosen.pinned), (Runner::Claude, false));

        rec.runner = Some(Runner::Codex);
        rec.launched = Some(Launched {
            runner: Some(Runner::Claude),
            model: "opus".into(),
            fallback: "sonnet".into(),
            effort: None,
        });
        let chosen = choose(&rec, &scopes(), &defaults(None));
        assert_eq!((chosen.runner, chosen.pinned), (Runner::Codex, true));
        assert_eq!(
            chosen.note.as_deref(),
            Some(
                "Runner: codex (from your message), model: opus (instance default), \
                 effort: default (instance default)"
            )
        );
        rec.launched.as_mut().unwrap().runner = Some(Runner::Codex);
        assert_eq!(choose(&rec, &scopes(), &defaults(None)).note, None);
        rec.launched.as_mut().unwrap().runner = None;
        assert_eq!(
            choose(&rec, &scopes(), &defaults(None)).note,
            None,
            "a turn from before runners were recorded"
        );
    }
}
