//! Working modes (`<home>/modes/<name>.md`): Linear labels pick one when a session starts, and
//! it adds instructions, a model, a permission mode and deny rules to every turn.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Permission modes Claude Code accepts for `--permission-mode`.
const PERMISSION_MODES: &[&str] = &[
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "default",
    "dontAsk",
    "manual",
    "plan",
];

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
    parse(name, &text).with_context(|| format!("in {}", path.display()))
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

/// A mode file: optional YAML frontmatter between `---` lines, then the instructions.
fn parse(name: &str, text: &str) -> Result<Mode> {
    let (front, body) = split_frontmatter(text)?;
    let mut mode = Mode {
        name: name.to_string(),
        instructions: body.trim().to_string(),
        ..Mode::default()
    };
    let mut seen: Vec<&str> = Vec::new();
    for (key, value, line) in frontmatter_fields(front)? {
        if seen.contains(&key) {
            bail!("frontmatter line {line}: `{key}` appears twice");
        }
        seen.push(key);
        let scalar = |value: Value| match value {
            Value::Scalar(s) if !s.is_empty() => Ok(s),
            _ => bail!("frontmatter line {line}: `{key}` takes one value"),
        };
        match key {
            "labels" => mode.labels = value.into_list(),
            "deny" => mode.deny = value.into_list(),
            "model" => mode.model = Some(scalar(value)?),
            "permission_mode" => {
                let pm = scalar(value)?;
                if !PERMISSION_MODES.contains(&pm.as_str()) {
                    bail!(
                        "frontmatter line {line}: permission_mode `{pm}` is not one of {}",
                        PERMISSION_MODES.join(", ")
                    );
                }
                mode.permission_mode = Some(pm);
            }
            other => bail!(
                "frontmatter line {line}: unknown key `{other}` (known: labels, model, \
                 permission_mode, deny)"
            ),
        }
    }
    Ok(mode)
}

/// The frontmatter (empty when there is none) and the body after it.
fn split_frontmatter(text: &str) -> Result<(&str, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some((first, rest)) = text.split_once('\n') else {
        return Ok(("", text));
    };
    if first.trim_end() != "---" {
        return Ok(("", text));
    }
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Ok((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    bail!("frontmatter has no closing `---` line")
}

#[derive(Debug, PartialEq)]
enum Value {
    Scalar(String),
    List(Vec<String>),
}

impl Value {
    fn into_list(self) -> Vec<String> {
        match self {
            Self::Scalar(s) if s.is_empty() => Vec::new(),
            Self::Scalar(s) => vec![s],
            Self::List(items) => items,
        }
    }
}

/// The subset of YAML mode files need: `key: value`, `key: [a, b]`, and `key:` followed by
/// `- item` lines. Each field comes with its 1-based line number within the frontmatter.
fn frontmatter_fields(front: &str) -> Result<Vec<(&str, Value, usize)>> {
    let mut fields: Vec<(&str, Value, usize)> = Vec::new();
    for (i, raw) in front.lines().enumerate() {
        let line = i + 1;
        let trimmed = strip_comment(raw).trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(item) = trimmed
            .strip_prefix("- ")
            .or((trimmed == "-").then_some(""))
        {
            match fields.last_mut() {
                Some((_, Value::List(items), _)) if raw.starts_with([' ', '\t', '-']) => {
                    let item = unquote(item);
                    if item.is_empty() {
                        bail!("frontmatter line {line}: empty list item");
                    }
                    items.push(item);
                }
                _ => bail!("frontmatter line {line}: list item outside a list"),
            }
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            bail!("frontmatter line {line}: expected `key: value`");
        };
        let value = value.trim();
        let value = if value.is_empty() {
            Value::List(Vec::new())
        } else if let Some(inner) = value.strip_prefix('[') {
            let Some(inner) = inner.strip_suffix(']') else {
                bail!("frontmatter line {line}: `[` without a closing `]`");
            };
            Value::List(
                split_items(inner)
                    .into_iter()
                    .map(unquote)
                    .filter(|s| !s.is_empty())
                    .collect(),
            )
        } else {
            Value::Scalar(unquote(value))
        };
        fields.push((key.trim(), value, line));
    }
    Ok(fields)
}

/// `line` without a trailing `# comment`: as in YAML, a `#` at the start or after
/// whitespace, outside quotes.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    let mut prev = ' ';
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') if opens_quote(prev) => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, '#') if prev.is_whitespace() => return &line[..i],
            _ => {}
        }
        prev = c;
    }
    line
}

/// The items of a `[a, b]` list, split at commas outside quotes and parentheses, so a rule
/// like `Bash(a, b)` stays one item.
fn split_items(inner: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let (mut depth, mut quote, mut start, mut prev) = (0usize, None, 0, ',');
    for (i, c) in inner.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') if opens_quote(prev) => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, '(') => depth += 1,
            (None, ')') => depth = depth.saturating_sub(1),
            (None, ',') if depth == 0 => {
                items.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        prev = c;
    }
    items.push(&inner[start..]);
    items
}

/// Whether a quote after `prev` starts a quoted value, rather than being an apostrophe
/// inside one, as in `don't`.
fn opens_quote(prev: char) -> bool {
    prev.is_whitespace() || matches!(prev, '[' | ',' | ':')
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return inner.to_string();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let text = "---\n\
                    # comment\n\
                    labels: [Research, \"deep dive\"]\n\
                    model: 'opus'\n\
                    permission_mode: plan\n\
                    deny:\n  - Edit\n  - Bash(git push:*)\n\
                    ---\n\nLook, do not touch.\n";
        let mode = parse("research", text).unwrap();
        assert_eq!(
            mode,
            Mode {
                name: "research".into(),
                labels: vec!["Research".into(), "deep dive".into()],
                model: Some("opus".into()),
                permission_mode: Some("plan".into()),
                deny: vec!["Edit".into(), "Bash(git push:*)".into()],
                instructions: "Look, do not touch.".into(),
            }
        );
    }

    #[test]
    fn frontmatter_is_optional() {
        let mode = parse("plain", "Just instructions.\n---\nmore").unwrap();
        assert_eq!(mode.instructions, "Just instructions.\n---\nmore");
        assert!(mode.labels.is_empty() && mode.model.is_none());

        let mode = parse("empty", "---\r\n---\r\nBody").unwrap();
        assert_eq!(mode.instructions, "Body");

        let mode = parse("one", "---\nlabels: Debug\ndeny:\n---\n").unwrap();
        assert_eq!(mode.labels, ["Debug"], "a single label without brackets");
        assert!(mode.deny.is_empty() && mode.instructions.is_empty());
    }

    #[test]
    fn rejects_malformed_frontmatter() {
        let err = |text: &str| format!("{:#}", parse("m", text).unwrap_err());
        assert!(err("---\nlabels: [a]\n").contains("no closing"));
        assert!(err("---\nlables: [a]\n---\n").contains("line 1: unknown key `lables`"));
        assert!(err("---\nmodel: a\nmodel: b\n---\n").contains("line 2: `model` appears twice"));
        assert!(err("---\nmodel: [a, b]\n---\n").contains("takes one value"));
        assert!(err("---\nmodel:\n---\n").contains("takes one value"));
        assert!(err("---\npermission_mode: yolo\n---\n").contains("not one of"));
        assert!(err("---\n- Edit\n---\n").contains("list item outside a list"));
        assert!(err("---\nmodel: x\n  - y\n---\n").contains("list item outside a list"));
        assert!(err("---\nlabels: [a\n---\n").contains("without a closing"));
        assert!(err("---\njust words\n---\n").contains("expected `key: value`"));
        assert!(err("---\ndeny:\n  - Edit\n  -\n---\n").contains("line 3: empty list item"));
        assert!(err("---\ndeny:\n  - ''\n---\n").contains("empty list item"));
        assert!(err("---\ndeny:\n  - # nothing\n---\n").contains("empty list item"));
    }

    #[test]
    fn strips_trailing_comments() {
        let text = "---\n\
                    labels: [Research] # picked by triage\n\
                    model: opus  # the big one\n\
                    deny:\n  - Edit # no edits\n  - \"Bash(echo #1)\"\n  - Bash(echo a#b)\n\
                    ---\n";
        let mode = parse("m", text).unwrap();
        assert_eq!(mode.labels, ["Research"]);
        assert_eq!(mode.model.as_deref(), Some("opus"));
        assert_eq!(mode.deny, ["Edit", "Bash(echo #1)", "Bash(echo a#b)"]);
        assert_eq!(strip_comment("model: don't # x"), "model: don't ");
    }

    #[test]
    fn accepts_bom_and_trailing_spaces_on_the_opening_line() {
        let mode = parse("m", "\u{feff}---  \nlabels: [A]\n--- \nBody").unwrap();
        assert_eq!(
            (mode.labels, mode.instructions),
            (vec!["A".into()], "Body".into())
        );
        let mode = parse("m", "\u{feff}No frontmatter").unwrap();
        assert_eq!(mode.instructions, "No frontmatter");
    }

    #[test]
    fn keeps_commas_inside_parentheses_and_quotes() {
        let mode = parse(
            "m",
            "---\ndeny: [Bash(a, b), 'x, y', Edit, Bash(f(1, 2), 3)]\n---\n",
        )
        .unwrap();
        assert_eq!(
            mode.deny,
            ["Bash(a, b)", "x, y", "Edit", "Bash(f(1, 2), 3)"]
        );
    }

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
