//! `[key=value]` directives in issue text and messages: `[repo=…]`, `[model=…]`, `[effort=…]`.

/// The values of every `[<key>=…]` in `text` that has a closing bracket, trimmed, in order.
pub fn values<'a>(text: &'a str, key: &str) -> impl Iterator<Item = &'a str> {
    let tag = format!("[{key}=");
    text.match_indices(&tag)
        .map(|(i, _)| i)
        .collect::<Vec<_>>()
        .into_iter()
        .filter_map(move |i| {
            let rest = &text[i + tag.len()..];
            Some(rest[..rest.find(']')?].trim())
        })
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
        assert_eq!(first("no directive", "repo"), None);
        assert_eq!(
            first("[model=sonnet] [effort=high]", "effort"),
            Some("high")
        );
    }

    #[test]
    fn lists_raw_values() {
        let found: Vec<_> =
            values("[model=] [model=<x>] [model= opus ] [model=", "model").collect();
        assert_eq!(found, ["", "<x>", "opus"]);
    }
}
