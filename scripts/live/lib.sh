#!/bin/sh
# Shared helpers for the live tests (sourced, not executed).
#
# The scripts run inside the dev container through scripts/dev.sh, from the
# repository root. Recordings and server data go under
# ${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}/live, never into the repo.
set -eu

repo_root() {
    cd "$(dirname "$0")/../.." && pwd -P
}

# The release binary, as scripts/selftest.sh finds it.
rec_binary() {
    if [ -n "${REC:-}" ]; then
        printf '%s' "$REC"
    elif [ -x "${CARGO_TARGET_DIR:-target}/release/rec-rs" ]; then
        printf '%s' "${CARGO_TARGET_DIR:-target}/release/rec-rs"
    else
        echo "live: no release binary; run 'scripts/dev.sh cargo build --release'" >&2
        return 1
    fi
}

# Where a live case writes everything it produces (MediaMTX's log, its
# recordings, the harness output). The selftest points this at the case's own
# folder, so a failure's server log and recordings sit next to the case report.
live_root() {
    printf '%s' "${REC_LIVE_DIR:-${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}/live}"
}

# wait_port PORT [SECS]
wait_port() {
    port=$1
    secs=${2:-20}
    i=0
    while [ "$i" -lt $((secs * 10)) ]; do
        if python3 - "$port" <<'EOF'
import socket, sys
s = socket.socket()
s.settimeout(0.2)
sys.exit(0 if s.connect_ex(("127.0.0.1", int(sys.argv[1]))) == 0 else 1)
EOF
        then
            return 0
        fi
        i=$((i + 1))
        sleep 0.1
    done
    echo "live: port $port did not open within ${secs}s" >&2
    return 1
}

# start_server DIR LOGFILE: start MediaMTX with its RTMP, RTMPS and WHIP
# listeners, wait for all three, and leave the pid in DIR/mediamtx.pid.
start_server() {
    dir=$1
    log=$2
    mkdir -p "$dir"
    scripts/live/mediamtx.sh serve "$dir" >"$log" 2>&1 &
    echo $! >"$dir/mediamtx.pid"
    wait_port 1935 30 && wait_port 1936 30 && wait_port 8889 30
}

stop_server() {
    dir=$1
    pid=$(cat "$dir/mediamtx.pid" 2>/dev/null || true)
    [ -n "$pid" ] || return 0
    kill -TERM "$pid" 2>/dev/null || true
    i=0
    while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 100 ]; do
        i=$((i + 1))
        sleep 0.1
    done
    kill -9 "$pid" 2>/dev/null || true
    rm -f "$dir/mediamtx.pid"
}

# newest_recording DIR: the newest .mp4 under DIR (empty when there is none).
newest_recording() {
    find "$1" -name '*.mp4' -type f 2>/dev/null | sort | tail -1
}

# check_recording NAME FILE MAX_MS: probe it and measure A/V sync.
# Prints "PASS  NAME  (detail)" or "FAIL  NAME  (detail)".
check_recording() {
    name=$1
    file=$2
    max_ms=$3
    rec=$(rec_binary)
    if [ -z "$file" ] || [ ! -s "$file" ]; then
        echo "FAIL  $name  (the server recorded nothing)"
        return 1
    fi
    if ! "$rec" probe "$file" >"$file.probe" 2>&1; then
        echo "FAIL  $name  (probe: $(tail -1 "$file.probe"))"
        return 1
    fi
    if ! python3 scripts/synccheck.py "$file" --max-ms "$max_ms" >"$file.check" 2>&1; then
        echo "FAIL  $name  ($(grep 'audio 0' "$file.check" | head -1))"
        return 1
    fi
    offset=$(grep 'audio 0' "$file.check" | head -1 | sed 's/.*offset //; s/,.*//')
    echo "PASS  $name  (A/V offset $offset, $(basename "$file"))"
    return 0
}
