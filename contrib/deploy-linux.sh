#!/usr/bin/env bash
# Rebuilds mothership on Linux and restarts its systemd user service without killing running
# turns: pull, build, drain, wait until idle, swap the binary, restart, check the commit.
# The macOS counterpart is contrib/deploy.sh; Linux has no TCC, so nothing is signed here.
#
# Every path and name is a variable; override any of them in the environment:
#   MOTHERSHIP_SRC=~/src/mothership MOTHERSHIP_BIN=~/.mothership/bin/mothership contrib/deploy-linux.sh
# FORCE=1 restarts without waiting for idle, killing any running turn.
set -euo pipefail

src=${MOTHERSHIP_SRC:-$(cd "$(dirname "$0")/.." && pwd)}
bin=${MOTHERSHIP_BIN:-$HOME/.mothership/bin/mothership}
unit=${SYSTEMD_UNIT:-mothership}
status_url=${STATUS_URL:-http://127.0.0.1:3456/status}
idle_timeout=${IDLE_TIMEOUT:-1800}

cd "$src"
git pull --ff-only
commit=$(git rev-parse --short HEAD)
# Reported by `/status` as `commit`, to check below that the new binary is the one answering.
MOTHERSHIP_COMMIT=$commit cargo build --release --locked

# A restart kills running agents, so wait until no turn is running. Meanwhile a draining
# mothership (SIGUSR1) holds new turns and runs them after the restart; if this script stops
# before restarting, SIGUSR2 lets the held turns start.
drained=
trap '[[ $drained == 1 ]] && systemctl --user kill -s USR2 --kill-whom=main "$unit"' EXIT
deadline=$((SECONDS + idle_timeout))
while [[ ${FORCE:-} != 1 ]]; do
    if ! status=$(curl -fsS "$status_url" 2>/dev/null); then
        echo "mothership is not answering on $status_url; nothing to wait for" >&2
        break
    fi
    if [[ -z $drained ]] && grep -q '"draining"' <<<"$status"; then
        # Only the main process: the agents in the unit's cgroup must not get the signal.
        systemctl --user kill -s USR1 --kill-whom=main "$unit"
        drained=1
        continue
    fi
    grep -q '"status":"idle"' <<<"$status" && break
    if ((SECONDS > deadline)); then
        echo "mothership still busy after ${idle_timeout}s; not restarting" >&2
        exit 1
    fi
    sleep 10
done

mkdir -p "$(dirname "$bin")"
cp target/release/mothership "$bin.new"
mv -f "$bin.new" "$bin"
# The new process starts undrained and runs the held turns itself.
drained=restarted
systemctl --user restart "$unit"
for _ in {1..30}; do
    if curl -fsS "$status_url" 2>/dev/null | grep -q "\"commit\":\"$commit\""; then
        echo "deployed $commit to $bin"
        exit 0
    fi
    sleep 1
done
echo "$bin was replaced, but $status_url does not report commit $commit within 30s" >&2
exit 1
