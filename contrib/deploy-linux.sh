#!/usr/bin/env bash
# Rebuilds mothership on Linux and restarts its systemd user service without killing running
# turns: pull, build, wait until idle and drain, swap the binary, restart, check the commit.
# The macOS counterpart is contrib/deploy.sh; Linux has no TCC, so nothing is signed here.
#
# Every path and name is a variable; override any of them in the environment:
#   MOTHERSHIP_SRC=~/src/mothership MOTHERSHIP_BIN=~/.mothership/bin/mothership contrib/deploy-linux.sh
# FORCE=1 restarts without waiting for idle, killing any running turn. IDLE_TIMEOUT (1800) bounds
# the wait for idle; HOLD_LIMIT (180) bounds how long a drain holds new turns.
set -euo pipefail

src=${MOTHERSHIP_SRC:-$(cd "$(dirname "$0")/.." && pwd)}
bin=${MOTHERSHIP_BIN:-$HOME/.mothership/bin/mothership}
unit=${SYSTEMD_UNIT:-mothership}
status_url=${STATUS_URL:-http://127.0.0.1:3456/status}
idle_timeout=${IDLE_TIMEOUT:-1800}
hold_limit=${HOLD_LIMIT:-180}

# shellcheck source=contrib/wait-idle.sh
source "$(cd "$(dirname "$0")" && pwd)/wait-idle.sh"

cd "$src"
git pull --ff-only
commit=$(git rev-parse --short HEAD)
# Reported by `/status` as `commit`, to check below that the new binary is the one answering.
MOTHERSHIP_COMMIT=$commit cargo build --release --locked

# A restart kills running agents, so wait until no turn is running, draining only once idle
# and for at most HOLD_LIMIT seconds (see contrib/wait-idle.sh). If this script stops while the
# drain holds turns, SIGUSR2 lets them start.
# Only the main process: the agents in the unit's cgroup must not get the signal.
drain() { systemctl --user kill -s USR1 --kill-whom=main "$unit"; }
undrain() { systemctl --user kill -s USR2 --kill-whom=main "$unit"; }
trap '[[ $drained == 1 ]] && undrain' EXIT
if [[ ${FORCE:-} != 1 ]]; then
    wait_idle
fi

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
