#!/usr/bin/env bash
# Everything a change must pass: file size, formatting, clippy (pedantic, warnings are errors), tests.
set -euo pipefail
cd "$(dirname "$0")/.."

# A file's logic is everything before `#[cfg(test)]` minus blank and `//` lines; it stays at 200
# lines or fewer. Files that predate the rule are pinned to their count so they cannot grow.
oversized="src/modes.rs:254"
too_long=0
while IFS= read -r file; do
    lines=$(awk '/^#\[cfg\(test\)\]/{exit} !/^[[:space:]]*(\/\/|$)/{n++} END{print n+0}' "$file")
    limit=200
    for entry in $oversized; do
        if [[ $entry == "$file":* ]]; then limit=${entry#*:}; fi
    done
    if ((lines > limit)); then
        echo "$file: $lines lines of logic, over the limit of $limit; split it by responsibility" >&2
        too_long=1
    fi
done < <(find src -name '*.rs' | sort)
if ((too_long)); then exit 1; fi

cargo fmt --check
cargo clippy --all-targets --quiet -- -D warnings
cargo test --quiet
for test in plugins/*/tests/*.test.sh; do
    "$test"
done
