//! Untrusted review text quoted to agents: closing tags that would end its quote block, and
//! whether it mentions the agent's account.

/// Escapes every `tag` (`</github_comment`), in any letter case, so quoted text cannot end the
/// block it is quoted in.
pub fn neutralize_closing_tag(text: &str, tag: &str) -> String {
    let tag = tag.to_ascii_lowercase();
    // ASCII lowercasing keeps byte offsets, so matches in `lower` index into `text`.
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (i, _) in lower.match_indices(&tag) {
        out.push_str(&text[last..i]);
        out.push_str("&lt;");
        out.push_str(&text[i + 1..i + tag.len()]);
        last = i + tag.len();
    }
    out.push_str(&text[last..]);
    out
}

/// Whether `text` mentions `name` (`@name`, in any letter case): neither side of the match may
/// be a character `name_char` says a name can hold, so `@name-2` is another account and
/// `x@name` an address.
pub fn mentions(text: &str, name: &str, name_char: fn(char) -> bool) -> bool {
    let lower = text.to_ascii_lowercase();
    let needle = format!("@{}", name.to_ascii_lowercase());
    lower.match_indices(&needle).any(|(i, _)| {
        let before = lower[..i].chars().next_back();
        let after = lower[i + needle.len()..].chars().next();
        !before.is_some_and(name_char) && !after.is_some_and(name_char)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_tag_is_neutralized_in_any_case() {
        assert_eq!(
            neutralize_closing_tag("a</GitHub_Comment>b</github_comment>", "</github_comment"),
            "a&lt;/GitHub_Comment>b&lt;/github_comment>"
        );
        assert_eq!(
            neutralize_closing_tag("a</Gerrit_Comment>b</github_comment>", "</gerrit_comment"),
            "a&lt;/Gerrit_Comment>b</github_comment>"
        );
        assert_eq!(neutralize_closing_tag("plain", "</github_comment"), "plain");
    }

    #[test]
    fn a_mention_ends_where_a_name_cannot_go_on() {
        let alnum = |c: char| c.is_ascii_alphanumeric();
        let dash = |c: char| c.is_ascii_alphanumeric() || c == '-';
        assert!(mentions("Hi @Bot.", "bot", alnum));
        assert!(
            mentions("@bot-2 or @bot", "bot", dash),
            "a later match counts"
        );
        assert!(mentions("@bot-2", "bot", alnum));
        assert!(!mentions("@bot-2", "bot", dash));
        assert!(!mentions("x@bot", "bot", alnum), "an address");
        assert!(!mentions("bot", "bot", alnum));
    }
}
