# Replaces the mothership binary, restarts it and rolls back if the new one does not answer;
# sourced by contrib/deploy.sh and contrib/deploy-linux.sh, which define `restart` for their
# service manager and `logs` (where the service's last log lines are) and call `keep_previous`
# before building and `swap_binary` after.
#
# `keep_previous` keeps the binary in place as `<bin>.prev`. It runs before the build because
# `bin` may be the build output itself, which the build overwrites with the new binary. If `/status` does
# not report the new commit within `answer_timeout` seconds, `.prev` goes back in place and the
# service restarts again, so a binary that dies at start does not leave the instance down.
#
# Inputs: bin, commit, status_url, answer_timeout. `keep_previous` needs only bin. `swap_binary <new binary>` returns 0 once the
# new commit answers and 1 otherwise, whether or not the rollback brought the old one back.

answer_timeout=${answer_timeout:-30}

# Waits for `/status` to answer with a body containing $1 (any answer when $1 is empty).
answers() {
    local i status
    for ((i = 0; i < answer_timeout; i++)); do
        if status=$(curl -fsS "$status_url" 2>/dev/null) && [[ $status == *"$1"* ]]; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# Puts $1 in place of the binary; the rename keeps a running process's file intact.
put() {
    cp "$1" "$bin.new"
    mv -f "$bin.new" "$bin"
}

keep_previous() {
    mkdir -p "$(dirname "$bin")"
    if [[ -e $bin ]]; then cp "$bin" "$bin.prev"; else rm -f "$bin.prev"; fi
}

swap_binary() {
    local new=$1 previous
    # What the running binary reports, so the rollback can check it is the one answering again.
    previous=$(curl -fsS "$status_url" 2>/dev/null | grep -o '"commit":"[^"]*"' || true)
    put "$new"
    restart || true
    if answers "\"commit\":\"$commit\""; then
        echo "deployed $commit to $bin"
        return 0
    fi
    echo "$bin was replaced, but $status_url does not report commit $commit within ${answer_timeout}s; its last log lines: $(logs)" >&2
    if [[ ! -e $bin.prev ]]; then
        echo "no previous binary to roll back to; the instance stays on the new one" >&2
        return 1
    fi
    put "$bin.prev"
    restart || true
    local was=${previous#\"commit\":\"}
    was=${was%\"}
    if answers "$previous"; then
        echo "rolled back to $bin.prev; $status_url answers again${previous:+ with commit $was}" >&2
    else
        echo "rolled back to $bin.prev, but $status_url does not answer${previous:+ with commit $was} within ${answer_timeout}s either; the instance is down: $(logs)" >&2
    fi
    return 1
}
