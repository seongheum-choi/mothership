#!/usr/bin/env bash
# Checks the Stop hook's decisions against throwaway git repositories.
set -euo pipefail

hook="$(cd "$(dirname "$0")/.." && pwd)/hooks/unshipped-work.sh"
# macOS `mktemp -d` ignores TMPDIR and uses the per-user temp directory, which a sandbox may not
# let it write; an explicit template keeps it in TMPDIR.
tmp=$(mktemp -d "${TMPDIR:-/tmp}/unshipped-work.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

# Run as part of a pre-commit hook, git exports GIT_INDEX_FILE and friends; without this the
# throwaway repositories below would write into the outer repository's index.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_PREFIX
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com
export GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com

failures=0

# [project_dir=<dir>] expect <name> <block|allow> <cwd> [stdin]
expect() {
    local name=$1 want=$2 dir=$3 input=${4:-'{"stop_hook_active": false}'} out got
    if [[ -n ${project_dir:-} ]]; then
        out=$(cd "$dir" && CLAUDE_PROJECT_DIR=$project_dir "$hook" <<<"$input")
    else
        out=$(cd "$dir" && env -u CLAUDE_PROJECT_DIR "$hook" <<<"$input")
    fi
    if [[ $out == *'"decision":"block"'* ]]; then got=block; else got=allow; fi
    if [[ $got == "$want" ]]; then
        echo "ok   $name"
    else
        echo "FAIL $name: want $want, got $got: $out"
        failures=$((failures + 1))
    fi
}

git init -q --bare "$tmp/origin.git"
git init -q -b main "$tmp/main"
git -C "$tmp/main" remote add origin "$tmp/origin.git"
echo one >"$tmp/main/file"
git -C "$tmp/main" add file
git -C "$tmp/main" commit -qm one
git -C "$tmp/main" push -q origin main
git -C "$tmp/main" fetch -q origin
git -C "$tmp/main" worktree add -q -b feature "$tmp/wt" origin/main --no-track
wt=$tmp/wt

mkdir "$tmp/plain"
expect "plain directory" allow "$tmp/plain"

echo dirty >>"$tmp/main/file"
expect "main checkout, even when dirty" allow "$tmp/main"
git -C "$tmp/main" checkout -q file

expect "clean worktree at origin" allow "$wt"

echo new >"$wt/untracked"
expect "untracked file only" allow "$wt"
rm "$wt/untracked"

echo two >>"$wt/file"
expect "modified tracked file" block "$wt"
expect "second Stop passes" allow "$wt" '{"session_id":"x","stop_hook_active":true}'
project_dir=$wt expect "CLAUDE_PROJECT_DIR wins over the current directory" block "$tmp/plain"
git -C "$wt" add file
expect "staged change" block "$wt"

git -C "$wt" commit -qm two
expect "commit on a branch with no upstream" block "$wt"

# Gerrit: a fake curl answers the change query from $tmp/gerrit-commits, once it got the
# account's credentials on stdin, escaped for curl's config syntax, and a query that leaves
# abandoned changes out.
mkdir "$tmp/bin"
cat >"$tmp/bin/curl" <<'FAKE'
#!/usr/bin/env bash
[[ $(cat) == 'user = "impala:p\"w"' ]] || exit 22
url=${*: -1}
[[ $url == *'+-is:abandoned' ]] || exit 22
sha=${url##*commit:}
sha=${sha%%+*}
printf ")]}'\n"
if grep -qx "$sha" "$GERRIT_COMMITS" 2>/dev/null; then echo '[{"_number":12}]'; else echo '[]'; fi
FAKE
chmod +x "$tmp/bin/curl"
saved_path=$PATH
export PATH="$tmp/bin:$PATH" GERRIT_URL=https://gerrit.test GERRIT_USERNAME=impala
export GERRIT_HTTP_PASSWORD='p"w' GERRIT_COMMITS="$tmp/gerrit-commits"
expect "commit Gerrit does not have" block "$wt"
git -C "$wt" rev-parse HEAD >"$GERRIT_COMMITS"
expect "commit pushed for review on Gerrit" allow "$wt"
git -C "$wt" commit -q --amend -m "two, amended"
expect "amended commit not pushed again" block "$wt"
export GERRIT_HTTP_PASSWORD=wrong
git -C "$wt" rev-parse HEAD >"$GERRIT_COMMITS"
expect "Gerrit refusing the query" block "$wt"
unset GERRIT_URL GERRIT_USERNAME GERRIT_HTTP_PASSWORD GERRIT_COMMITS
export PATH=$saved_path

git -C "$wt" push -q origin feature
expect "pushed branch" allow "$wt"

git -C "$wt" commit -q --allow-empty -m three
expect "commit ahead of pushed branch" block "$wt"

git -C "$wt" checkout -q --detach origin/main
expect "detached at a remote commit" allow "$wt"

if ((failures > 0)); then
    echo "$failures failure(s)"
    exit 1
fi
