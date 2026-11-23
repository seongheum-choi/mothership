#!/usr/bin/env bash
# Runs contrib/wait-idle.sh against a fake `/status` and a fake clock, and checks that a drain
# never holds new turns for longer than the hold limit and that idle still ends in a restart.
set -euo pipefail

# shellcheck source=contrib/wait-idle.sh
source "$(cd "$(dirname "$0")/.." && pwd)/wait-idle.sh"

failures=0
status_url=http://fake/status
idle_timeout=1800
hold_limit=180
poll=10

# The fake instance: busy from `busy_from` until `busy_until` on the fake clock, `SECONDS`.
# `answering=0` refuses connections; `old=1` is a binary that reports no `draining`.
curl() {
    ((answering)) || return 7
    local state=idle
    ((SECONDS >= busy_from && SECONDS < busy_until)) && state=busy
    if ((old)); then
        echo "{\"status\":\"$state\"}"
    else
        echo "{\"status\":\"$state\",\"draining\":$( ((draining)) && echo true || echo false)}"
    fi
}
sleep() { SECONDS=$((SECONDS + $1)); }
drain() {
    draining=1 drains=$((drains + 1)) drain_start=$SECONDS
    # A turn that started just before the drain took hold.
    if ((slip > 0)); then busy_from=$SECONDS busy_until=$((SECONDS + slip)) slip=0; fi
}
undrain() {
    draining=0
    local held=$((SECONDS - drain_start))
    ((held > longest_hold)) && longest_hold=$held
    return 0
}

# expect <name> <busy_until> <want result> <want drained> <want drains> [slip] [answering] [old]
expect() {
    local name=$1 result=0
    SECONDS=0 busy_from=0 busy_until=$2 slip=${6:-0} answering=${7:-1} old=${8:-0}
    drained= draining=0 drains=0 drain_start=0 longest_hold=0
    wait_idle 2>/dev/null || result=$?
    if ((draining)); then undrain; fi
    local got="result=$result drained=${drained:-0} drains=$drains"
    local want="result=$3 drained=$4 drains=$5"
    if [[ $got != "$want" ]]; then
        echo "FAIL $name: want $want, got $got"
        failures=$((failures + 1))
    elif ((longest_hold > hold_limit + poll)); then
        echo "FAIL $name: held new turns for ${longest_hold}s"
        failures=$((failures + 1))
    else
        echo "ok   $name (longest hold ${longest_hold}s)"
    fi
}

expect "idle at once drains and restarts" 0 0 1 1
expect "a turn that ends in time is waited for undrained" 600 0 1 1
expect "a turn longer than the timeout holds nothing and fails" 4000 1 0 0
expect "a turn that slipped in before the drain is held at most the limit" 0 0 1 2 1000
expect "a slipped-in turn past the timeout fails undrained" 0 1 0 1 5000
expect "an instance that does not answer is restarted" 0 0 0 0 0 0
expect "a binary without draining is not signalled" 0 0 0 0 0 1 1

if ((failures)); then
    echo "$failures failed" >&2
    exit 1
fi
