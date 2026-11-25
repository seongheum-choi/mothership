#!/usr/bin/env bash
# Rebuilds mothership on macOS and restarts its launchd agent without breaking the privacy
# (TCC) approval: pull, build, sign with a fixed identity, wait until idle and drain, swap the
# binary, kickstart, and roll back if it does not answer. See "Agent does not start (macOS
# privacy prompts)" in the README.
#
# Every path and name is a variable; override any of them in the environment:
#   MOTHERSHIP_SRC=~/src/mothership MOTHERSHIP_BIN=~/.local/bin/mothership contrib/deploy.sh
# FORCE=1 restarts without waiting for idle, killing any running turn. IDLE_TIMEOUT (1800) bounds
# the wait for idle; HOLD_LIMIT (180) bounds how long a drain holds new turns.
set -euo pipefail

src=${MOTHERSHIP_SRC:-$HOME/mothership}
bin=${MOTHERSHIP_BIN:-$HOME/.mothership/bin/mothership}
label=${LAUNCHD_LABEL:-com.mothership.agent}
status_url=${STATUS_URL:-http://127.0.0.1:3456/status}
identity=${SIGN_IDENTITY:-Mothership Dev}
keychain=${SIGN_KEYCHAIN:-$HOME/Library/Keychains/mothership.keychain-db}
# Optional: a file holding the keychain password, so the script runs unattended.
keychain_pw=${SIGN_KEYCHAIN_PW_FILE:-$HOME/.mothership/.keychain-pw}
idle_timeout=${IDLE_TIMEOUT:-1800}
hold_limit=${HOLD_LIMIT:-180}

# shellcheck source=contrib/wait-idle.sh
source "$(cd "$(dirname "$0")" && pwd)/wait-idle.sh"
# shellcheck source=contrib/swap-binary.sh
source "$(cd "$(dirname "$0")" && pwd)/swap-binary.sh"

cd "$src"
git pull --ff-only
commit=$(git rev-parse --short HEAD)
# Before the build, which overwrites the binary in place when MOTHERSHIP_BIN is its output.
keep_previous
# Reported by `/status` as `commit`, to check below that the new binary is the one answering.
MOTHERSHIP_COMMIT=$commit cargo build --release --locked

if [[ -r $keychain_pw ]]; then
    if [[ $(stat -f %Lp "$keychain_pw") != 600 ]]; then
        echo "$keychain_pw must be mode 0600" >&2
        exit 1
    fi
    security unlock-keychain -p "$(cat "$keychain_pw")" "$keychain"
fi
# A self-signed certificate is not "valid" to find-identity -v, so list all identities.
if ! security find-identity -p codesigning "$keychain" 2>/dev/null | grep -q "\"$identity\""; then
    echo "no code-signing identity \"$identity\" in $keychain; see README \"Signing and deploying\"" >&2
    exit 1
fi
# TCC remembers a program by its identifier and signing certificate, so a fixed identity and
# identifier keep the approval across rebuilds; the linker's ad-hoc signature changes every time.
codesign --force --sign "$identity" --keychain "$keychain" --identifier mothership \
    target/release/mothership
codesign --verify target/release/mothership

# A restart kills running agents, so wait for idle first (see contrib/wait-idle.sh); the EXIT
# trap ends a drain this script leaves behind.
service=gui/$(id -u)/$label
drain() { launchctl kill SIGUSR1 "$service"; }
undrain() { launchctl kill SIGUSR2 "$service"; }
trap '[[ $drained == 1 ]] && undrain' EXIT
if [[ ${FORCE:-} != 1 ]]; then
    wait_idle
fi

restart() { launchctl kickstart -k "$service"; }
# launchd's stderr log, which holds what a binary that dies at start printed.
logs() {
    local path
    path=$(launchctl print "$service" 2>/dev/null | awk -F' = ' '/stderr path/ {print $2; exit}' || true)
    echo "tail -n 20 ${path:-<StandardErrorPath of $label>}"
}
# The new process starts undrained and runs the held turns itself.
drained=restarted
swap_binary target/release/mothership
