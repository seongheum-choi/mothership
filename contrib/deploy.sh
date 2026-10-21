#!/usr/bin/env bash
# Rebuilds mothership on macOS and restarts its launchd agent without breaking the privacy
# (TCC) approval: pull, build, sign with a fixed identity, wait until idle, swap the binary,
# kickstart. See "Agent does not start (macOS privacy prompts)" in the README.
#
# Every path and name is a variable; override any of them in the environment:
#   MOTHERSHIP_SRC=~/src/mothership MOTHERSHIP_BIN=~/.local/bin/mothership contrib/deploy.sh
# FORCE=1 restarts without waiting for idle, killing any running turn.
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

cd "$src"
git pull --ff-only
commit=$(git rev-parse --short HEAD)
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

# A restart kills running agents, so wait until no turn is running. Meanwhile a mothership that
# reports `draining` holds new turns (SIGUSR1) and runs them after the restart, so the running
# ones are the last; if this script stops before restarting, SIGUSR2 lets the held turns start.
service=gui/$(id -u)/$label
drained=
trap '[[ $drained == 1 ]] && launchctl kill SIGUSR2 "$service"' EXIT
deadline=$((SECONDS + idle_timeout))
while [[ ${FORCE:-} != 1 ]]; do
    if ! status=$(curl -fsS "$status_url" 2>/dev/null); then
        echo "mothership is not answering on $status_url; nothing to wait for" >&2
        break
    fi
    if [[ -z $drained ]] && grep -q '"draining"' <<<"$status"; then
        launchctl kill SIGUSR1 "$service"
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
launchctl kickstart -k "$service"
for _ in {1..30}; do
    if curl -fsS "$status_url" 2>/dev/null | grep -q "\"commit\":\"$commit\""; then
        echo "deployed $commit to $bin"
        exit 0
    fi
    sleep 1
done
echo "$bin was replaced, but $status_url does not report commit $commit within 30s" >&2
exit 1
