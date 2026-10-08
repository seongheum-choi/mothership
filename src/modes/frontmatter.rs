//! Mode files: optional YAML frontmatter between `---` lines, then the instructions. Reads
//! the subset of YAML they need, with comments, quotes and a byte order mark.

use super::Mode;
use anyhow::{Result, bail};

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

/// A mode file: optional YAML frontmatter between `---` lines, then the instructions.
pub(super) fn parse(name: &str, text: &str) -> Result<Mode> {
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
}
