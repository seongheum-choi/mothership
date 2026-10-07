#!/usr/bin/env bash
# Stop hook: sends the agent back once if its worktree holds uncommitted tracked changes or
# commits that no remote has, so work is shipped or the reason for leaving it is stated.
set -uo pipefail

input=$(cat)

# A Stop that follows this hook's own block always passes, so the agent cannot loop.
if [[ $input =~ \"stop_hook_active\"[[:space:]]*:[[:space:]]*true ]]; then
    exit 0
fi

dir=${CLAUDE_PROJECT_DIR:-$PWD}

# Only linked worktrees (issue sessions). Plain directories such as Zulip workspaces, and
# main checkouts, have no work this hook should chase.
git_dir=$(git -C "$dir" rev-parse --absolute-git-dir 2>/dev/null) || exit 0
common_dir=$(cd "$dir" && cd "$(git rev-parse --git-common-dir)" && pwd -P) || exit 0
[[ $(cd "$git_dir" && pwd -P) != "$common_dir" ]] || exit 0

changed=$(git -C "$dir" status --porcelain --untracked-files=no | wc -l | tr -d ' ')
# Branches start without an upstream, so count commits no remote-tracking ref contains.
unpushed=$(git -C "$dir" rev-list --count HEAD --not --remotes 2>/dev/null || echo 0)

if ((changed == 0 && unpushed == 0)); then
    exit 0
fi

found=()
((changed > 0)) && found+=("${changed} uncommitted tracked file(s)")
((unpushed > 0)) && found+=("${unpushed} commit(s) not pushed to any remote")
summary=$(printf '%s and ' "${found[@]}")
summary=${summary% and }

printf '{"decision":"block","reason":"This worktree has %s. Finish the work (commit and push), or say why it is being left unshipped."}\n' "$summary"
