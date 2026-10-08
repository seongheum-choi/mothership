//! Working modes (`<home>/modes/<name>.md`): Linear labels pick one when a session starts, and
//! it adds instructions, a model, a permission mode and deny rules to every turn.

mod frontmatter;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Default)]
pub struct Mode {
    /// The file name without `.md`.
    pub name: String,
    /// Linear label names that turn this mode on, compared case-insensitively.
    pub labels: Vec<String>,
    /// Replaces `CLAUDE_MODEL`.
    pub model: Option<String>,
    /// Replaces the issue sessions' `bypassPermissions`.
    #[expect(
        clippy::struct_field_names,
        reason = "named after the frontmatter key and Claude Code's --permission-mode"
    )]
    pub permission_mode: Option<String>,
    /// Claude Code permission deny rules, added to the sandbox's.
    pub deny: Vec<String>,
    /// The file's body, appended to the system prompt.
    pub instructions: String,
}

/// The mode a session settled on and the issue label that picked it.
#[derive(Debug, PartialEq)]
pub struct Choice<'a> {
    pub mode: &'a Mode,
    pub label: String,
}

pub fn dir(home: &Path) -> PathBuf {
    home.join("modes")
}

/// Every mode in `dir`, by name. No directory means no modes; one broken file fails them
/// all, since it might be the one the issue's labels ask for.
pub fn load_all(dir: &Path) -> Result<Vec<Mode>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            let is_md = path.extension().is_some_and(|x| x == "md") && path.is_file();
            is_md
                .then(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
                .flatten()
        })
        .collect();
    names.sort();
    names.iter().map(|name| load(dir, name)).collect()
}

/// Reads `<dir>/<name>.md`, which turns re-read so edits apply without a restart.
pub fn load(dir: &Path, name: &str) -> Result<Mode> {
    let path = dir.join(format!("{name}.md"));
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    frontmatter::parse(name, &text).with_context(|| format!("in {}", path.display()))
}

/// Picks the mode whose `labels` include one of the issue's. `Ok(None)` when none does; an
/// error naming the modes and labels when several do, so nobody's guess decides.
pub fn select<'a>(
    modes: &'a [Mode],
    issue_labels: &[String],
) -> Result<Option<Choice<'a>>, String> {
    let matches: Vec<Choice> = modes
        .iter()
        .filter_map(|mode| {
            issue_labels
                .iter()
                .find(|l| mode.labels.iter().any(|m| m.eq_ignore_ascii_case(l)))
                .map(|label| Choice {
                    mode,
                    label: label.clone(),
                })
        })
        .collect();
    let mut matches = matches.into_iter();
    match (matches.next(), matches.len()) {
        (None, _) => Ok(None),
        (Some(only), 0) => Ok(Some(only)),
        (Some(first), _) => {
            let list = std::iter::once(first)
                .chain(matches)
                .map(|c| format!("`{}` (label `{}`)", c.mode.name, c.label))
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "The issue's labels select more than one mode: {list}. Leave the label of one \
                 mode and reply to start."
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(name: &str, labels: &[&str]) -> Mode {
        Mode {
            name: name.into(),
            labels: labels.iter().map(|l| (*l).to_string()).collect(),
            ..Mode::default()
        }
    }

    #[test]
    fn selects_by_label_ignoring_case() {
        let modes = [
            mode("implement", &["Implement", "Feature"]),
            mode("research", &["research"]),
            mode("unlabelled", &[]),
        ];
        let labels = |ls: &[&str]| ls.iter().map(|l| (*l).to_string()).collect::<Vec<_>>();
        let pick = |ls: &[&str]| {
            select(&modes, &labels(ls)).map(|c| c.map(|c| (c.mode.name.clone(), c.label)))
        };

        assert_eq!(pick(&[]), Ok(None));
        assert_eq!(pick(&["backend"]), Ok(None));
        assert_eq!(
            pick(&["backend", "RESEARCH"]),
            Ok(Some(("research".into(), "RESEARCH".into())))
        );
        assert_eq!(
            pick(&["feature", "implement"]),
            Ok(Some(("implement".into(), "feature".into()))),
            "two labels of one mode are not a conflict"
        );
        let err = pick(&["Research", "Implement"]).unwrap_err();
        assert!(
            err.contains("`implement` (label `Implement`), `research` (label `Research`)"),
            "{err}"
        );
    }

    #[test]
    fn bundled_modes_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("contrib/modes");
        let modes = load_all(&dir).unwrap();
        let names: Vec<&str> = modes.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["debug", "implement", "research"]);
        assert!(modes.iter().all(|m| !m.labels.is_empty()));
        assert!(modes[2].deny.contains(&"Edit".to_string()));
        assert_eq!(modes[2].permission_mode.as_deref(), Some("dontAsk"));
    }

    #[test]
    fn loads_md_files_by_name() {
        let dir = std::env::temp_dir().join(format!("mothership-modes-{}", std::process::id()));
        assert!(load_all(&dir).unwrap().is_empty(), "no directory, no modes");
        std::fs::create_dir_all(dir.join("sub.md")).unwrap();
        std::fs::write(dir.join("b.md"), "---\nlabels: [B]\n---\nbee").unwrap();
        std::fs::write(dir.join("a.md"), "ay").unwrap();
        std::fs::write(dir.join("notes.txt"), "---\nbroken").unwrap();
        let names: Vec<String> = load_all(&dir)
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(load(&dir, "b").unwrap().instructions, "bee");

        std::fs::write(dir.join("c.md"), "---\nbroken").unwrap();
        let err = format!("{:#}", load_all(&dir).unwrap_err());
        assert!(err.contains("c.md") && err.contains("no closing"), "{err}");
        assert!(load(&dir, "gone").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
