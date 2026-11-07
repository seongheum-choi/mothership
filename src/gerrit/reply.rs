//! The `gerrit-reply` wrapper agents answer review on Gerrit with.

use anyhow::{Context, Result};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

/// The review tag `gerrit-reply` posts with. Gerrit shows a tag on nothing, unlike an HTML
/// marker in the text, and copies it to the review's message and comments, so mothership can
/// tell the agent's own replies from new feedback.
pub(super) const TAG: &str = "mothership";

/// Writes `<home>/gerrit-reply/gerrit-reply` and returns its directory, which goes first on
/// agents' `PATH`.
pub(super) fn install(home: &Path) -> Result<PathBuf> {
    // A directory of its own: it goes first on agents' PATH, so nothing else may live there.
    let bin_dir = home.join("gerrit-reply");
    std::fs::create_dir_all(&bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;
    let script = bin_dir.join("gerrit-reply");
    std::fs::write(&script, reply_script())
        .with_context(|| format!("writing {}", script.display()))?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("making {} executable", script.display()))?;
    Ok(bin_dir)
}

/// `gerrit-reply <change> <patch-set> [<comment-id> <path> [<line>]]`, body on stdin: a reply
/// that resolves the comment's thread, or else a review message on the patch set. Posted to
/// `POST /a/changes/{change}/revisions/{patch-set}/review` with [`TAG`], with the credentials
/// from the environment, which reach curl on its standard input rather than its command line.
fn reply_script() -> String {
    format!(
        r#"#!/bin/sh
# Installed by mothership. Posts a Gerrit review reply tagged so that mothership does not take
# it for new feedback.
set -eu
if [ $# -ne 2 ] && [ $# -ne 4 ] && [ $# -ne 5 ]; then
    echo "usage: gerrit-reply <change> <patch-set> [<comment-id> <path> [<line>]] < body" >&2
    exit 2
fi
text="$(cat)"
if [ -z "$(printf '%s' "$text" | tr -d '[:space:]')" ]; then
    echo "gerrit-reply: empty reply, nothing posted" >&2
    exit 2
fi
if [ $# -eq 2 ]; then
    review=$(jq -n --arg m "$text" '{{tag: "{TAG}", message: $m}}')
else
    review=$(jq -n --arg m "$text" --arg id "$3" --arg path "$4" --arg line "${{5:-}}" '
        {{in_reply_to: $id, message: $m, unresolved: false}}
        + (if $line == "" then {{}} else {{line: ($line | tonumber)}} end)
        | {{tag: "{TAG}", comments: {{($path): [.]}}}}')
fi
# curl's config syntax: a backslash or double quote in a credential is escaped.
quote() {{ printf '%s' "$1" | sed 's/[\\"]/\\&/g'; }}
if ! out=$(printf 'user = "%s:%s"\n' "$(quote "$GERRIT_USERNAME")" "$(quote "$GERRIT_HTTP_PASSWORD")" |
    curl --silent --show-error --fail-with-body --config - \
        --header 'Content-Type: application/json' --data-binary "$review" \
        "$GERRIT_URL/a/changes/$1/revisions/$2/review"); then
    printf 'gerrit-reply: %s\n' "$out" >&2
    exit 1
fi
"#
    )
}
