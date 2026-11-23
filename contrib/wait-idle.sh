# Waits until mothership runs no turn and holds new ones, so a restart kills nothing; sourced by
# contrib/deploy.sh and contrib/deploy-linux.sh, which define `drain` (send SIGUSR1) and `undrain`
# (send SIGUSR2) for their service manager and restart once `wait_idle` returns.
#
# A drain holds every new Linear turn, so it is never held for longer than `hold_limit`: the wait
# runs undrained until `/status` reports idle, drains then, and restarts if it is still idle. A
# turn that started just before the drain gets `hold_limit` seconds to finish; after that the
# drain ends, the held turns start, and the wait for idle begins again. `idle_timeout` bounds the
# whole wait; past it `wait_idle` fails and leaves nothing held.
#
# Inputs: status_url, idle_timeout, hold_limit, poll (seconds between looks). Sets `drained` to 1
# while the instance drains, so the caller's EXIT trap can end a drain this script leaves behind.

drained=
poll=${poll:-10}

wait_idle() {
    local status deadline=$((SECONDS + idle_timeout)) held_since=0
    while true; do
        if ! status=$(curl -fsS "$status_url" 2>/dev/null); then
            echo "mothership is not answering on $status_url; nothing to wait for" >&2
            return 0
        fi
        if [[ $status == *'"status":"idle"'* ]]; then
            [[ $drained == 1 ]] && return 0
            # A binary without `draining` would die of SIGUSR1; idle is enough for it.
            [[ $status == *'"draining"'* ]] || return 0
            drain
            drained=1
            held_since=$SECONDS
            # Look again at once: a turn may have started between that look and the drain.
            continue
        fi
        if [[ $drained == 1 ]] && ((SECONDS - held_since >= hold_limit)); then
            echo "a turn is still running after ${hold_limit}s of draining; running the held turns and waiting for idle again" >&2
            undrain
            drained=
        fi
        if ((SECONDS >= deadline)); then
            echo "mothership still busy after ${idle_timeout}s; not restarting" >&2
            return 1
        fi
        sleep "$poll"
    done
}
