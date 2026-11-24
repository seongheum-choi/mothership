#!/usr/bin/env bash
# Runs contrib/swap-binary.sh against a fake service and fake binaries, and checks that a good
# binary deploys as before and that one which never answers is rolled back to the old one.
set -euo pipefail

# shellcheck source=contrib/swap-binary.sh
source "$(cd "$(dirname "$0")/.." && pwd)/swap-binary.sh"

failures=0
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
bin=$dir/bin/mothership
status_url=http://fake/status

# A fake binary is a file holding the commit it reports, or `dies` for one that exits at start.
# `restart` runs what is in place; `curl` answers as that process, or refuses when none runs.
restart() { running=$(cat "$bin" 2>/dev/null || echo dies); }
curl() {
    [[ $running != dies ]] || return 7
    echo "{\"status\":\"idle\",\"commit\":\"$running\"}"
}
logs() { echo "journalctl --user -u fake"; }
sleep() { :; }

# expect <name> <old binary or -> <new binary> <want result> <want running> <want output>
expect() {
    local name=$1 result=0 output
    rm -rf "$dir/bin" "$dir/new"
    running=dies
    if [[ $2 != - ]]; then
        mkdir -p "$dir/bin"
        echo "$2" >"$bin"
        restart
    fi
    echo "$3" >"$dir/new"
    commit=abc1234
    # Not in $(...): its subshell would keep `running` from the test.
    swap_binary "$dir/new" >"$dir/out" 2>&1 || result=$?
    output=$(<"$dir/out")
    local got="result=$result running=$running"
    local want="result=$4 running=$5"
    if [[ $got != "$want" ]]; then
        echo "FAIL $name: want $want, got $got"
        failures=$((failures + 1))
    elif [[ $output != *"$6"* ]]; then
        echo "FAIL $name: output lacks \"$6\": $output"
        failures=$((failures + 1))
    else
        echo "ok   $name"
    fi
}

expect "a binary that answers is deployed" old0000 abc1234 0 abc1234 "deployed abc1234"
expect "a binary that dies is rolled back to the old one" old0000 dies 1 old0000 \
    "answers again with commit old0000"
expect "the failure names where the log is" old0000 dies 1 old0000 "journalctl --user -u fake"
expect "an old binary that dies too is reported down" dies dies 1 dies "the instance is down"
expect "the first deploy has nothing to roll back to" - dies 1 dies "no previous binary"
expect "the first deploy that answers is deployed" - abc1234 0 abc1234 "deployed abc1234"

if ((failures)); then
    echo "$failures failed" >&2
    exit 1
fi
