//! `[key=value]` directives in issue text and messages: `[repo=…]`, `[runner=…]`, `[model=…]`,
//! `[effort=…]`.

use std::ops::Range;

/// The values of every `[<key>=…]` in `text` that has a closing bracket and is outside code
/// spans and fenced blocks, trimmed, in order. Text that quotes a directive in backticks is
/// talking about it, as an issue about routing does, not giving it.
pub fn values<'a>(text: &'a str, key: &str) -> impl Iterator<Item = &'a str> {
    let tag = format!("[{key}=");
    let code = code_spans(text);
    text.match_indices(&tag)
        .map(|(i, _)| i)
        .filter(|i| !code.iter().any(|c| c.contains(i)))
        .collect::<Vec<_>>()
        .into_iter()
        .filter_map(move |i| {
            let rest = &text[i + tag.len()..];
            Some(rest[..rest.find(']')?].trim())
        })
}

/// Byte ranges of the code in Markdown `text`: a run of backticks through the next run of the
/// same length, which covers both `` `inline` `` spans and ```` ``` ```` fences. A run with no
/// closing run is literal.
fn code_spans(text: &str) -> Vec<Range<usize>> {
    let run = |at: usize| text[at..].bytes().take_while(|&b| b == b'`').count();
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some(start) = text[from..].find('`').map(|o| from + o) {
        let len = run(start);
        from = start + len;
        let mut at = from;
        while let Some(close) = text[at..].find('`').map(|o| at + o) {
            let close_len = run(close);
            at = close + close_len;
            if close_len == len {
                spans.push(start..at);
                from = at;
                break;
            }
        }
    }
    spans
}

/// The first `[<key>=<value>]` of `text` whose value could be a name, so a placeholder like
/// `[repo=<name>]` in prose is passed over.
pub fn first<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    values(text, key).find(|v| valid_name(v))
}

/// Letters, digits, `-`, `_` and `.`: what repository and model names are made of.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_first_named_value() {
        assert_eq!(first("Fix it.\n[repo= vault ]", "repo"), Some("vault"));
        assert_eq!(first("[repo=]", "repo"), None);
        assert_eq!(first("add `[repo=<name>]` or `[repo=…]`", "repo"), None);
        assert_eq!(
            first("`[repo=<name>]`, here: [repo=app]", "repo"),
            Some("app")
        );
        assert_eq!(first("[repo=open", "repo"), None);
        assert_eq!(
            first("e.g. `[repo=app]`", "repo"),
            None,
            "quoted, not given"
        );
        assert_eq!(first("no directive", "repo"), None);
        assert_eq!(
            first("[model=sonnet] [effort=high]", "effort"),
            Some("high")
        );
    }

    #[test]
    fn skips_code_spans_and_fences() {
        fn found(text: &str) -> Vec<&str> {
            values(text, "repo").collect()
        }
        assert_eq!(
            found("`[repo=a]` [repo=b] ``x `[repo=c]` y`` [repo=d]"),
            ["b", "d"]
        );
        assert_eq!(found("```\n[repo=a]\n```\n[repo=b]"), ["b"]);
        assert_eq!(
            found("it's a ` tick [repo=a]"),
            ["a"],
            "an unclosed backtick"
        );
        assert_eq!(
            found("``[repo=a]` [repo=b]"),
            ["a", "b"],
            "runs of other lengths"
        );
        assert_eq!(code_spans("a `b` c `d`"), [2..5, 8..11]);
    }

    #[test]
    fn lists_raw_values() {
        let found: Vec<_> =
            values("[model=] [model=<x>] [model= opus ] [model=", "model").collect();
        assert_eq!(found, ["", "<x>", "opus"]);
    }
}
