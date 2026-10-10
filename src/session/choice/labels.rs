//! `model:<name>` and `effort:<level>` labels on a Linear issue, read once when its session
//! starts. A label that cannot be used is logged and left unset, so a labelling mistake never
//! stops a session.

use crate::config::check_effort;

/// The labels, as written, that set the session's model and effort.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Picked {
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// The value of a `<prefix>:<value>` label; the labels stored on a session have been checked.
pub(super) fn value(label: &str) -> Option<&str> {
    let (_, value) = label.split_once(':')?;
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

/// The `model:` and `effort:` labels of `labels`, matched case-insensitively on the prefix.
pub fn select(key: &str, labels: &[String]) -> Picked {
    let effort = one(key, labels, "effort").filter(|label| {
        let level = value(label).unwrap_or_default();
        match check_effort(level) {
            Ok(()) => true,
            Err(problem) => {
                tracing::warn!("[{key}] label `{label}` ignored: {problem}");
                false
            }
        }
    });
    Picked {
        model: one(key, labels, "model"),
        effort,
    }
}

/// The one label of `labels` that starts with `prefix:` and names a value. Two of them are a
/// conflict nobody's guess should decide, so neither is used.
fn one(key: &str, labels: &[String], prefix: &str) -> Option<String> {
    let mut found = labels
        .iter()
        .filter(|label| {
            label
                .split_once(':')
                .is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case(prefix))
        })
        .filter(|label| {
            value(label).is_some() || {
                tracing::warn!("[{key}] label `{label}` ignored: it names no {prefix}");
                false
            }
        });
    let first = found.next()?;
    if let Some(second) = found.next() {
        tracing::warn!(
            "[{key}] labels `{first}` and `{second}` both set the {prefix}; using neither"
        );
        return None;
    }
    Some(first.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(ls: &[&str]) -> Vec<String> {
        ls.iter().map(|l| (*l).to_string()).collect()
    }

    #[test]
    fn picks_one_label_per_prefix() {
        assert_eq!(select("s1", &labels(&[])), Picked::default());
        assert_eq!(
            select("s1", &labels(&["backend", "Model:sonnet", "effort: high"])),
            Picked {
                model: Some("Model:sonnet".into()),
                effort: Some("effort: high".into()),
            }
        );
        assert_eq!(value("Model:sonnet"), Some("sonnet"));
        assert_eq!(value("effort: high"), Some("high"));
    }

    #[test]
    fn a_bad_or_doubled_label_is_left_unset() {
        assert_eq!(select("s1", &labels(&["effort:ultra"])), Picked::default());
        assert_eq!(select("s1", &labels(&["effort:HIGH"])), Picked::default());
        assert_eq!(select("s1", &labels(&["model:"])), Picked::default());
        assert_eq!(
            select("s1", &labels(&["model:opus", "model:haiku"])),
            Picked::default()
        );
        assert_eq!(
            select("s1", &labels(&["model:opus", "effort:ultra"])),
            Picked {
                model: Some("model:opus".into()),
                effort: None,
            },
            "one bad label leaves the other alone"
        );
    }
}
