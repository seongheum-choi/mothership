//! `[model=…]` and `[effort=…]` in a session's messages, recorded on the session so the turns
//! that start after the message run with them. `default` gives the field back to the wider
//! scopes.

use crate::{app::App, config::EFFORTS, directive, store::Replaced};

/// What one message's directive does to a field.
#[derive(Debug, Default, PartialEq)]
enum Set {
    #[default]
    Keep,
    /// `default`: back to the labels, the mode, the repository and the instance.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
