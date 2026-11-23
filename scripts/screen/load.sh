#!/bin/sh
# Repeat the screen-capture self-test cases under CPU load, to catch races.
#
#   scripts/screen/load.sh 10 pattern-screen screen-region screen-token \
#       screen-x11 screen-window screen-exclude
#
# Every round starts each case from scratch (its own headless/sway or Xvfb
# session), exactly like `scripts/selftest.sh` does, while LOAD_BURNERS
# (default 8) `yes > /dev/null` processes keep the CPU busy: the recorder then
# competes with the compositor, the portal and PipeWire the way it does on a
# loaded host.
#
# Logs (the record's own stderr with REC_DEBUG=1 when you set it, the harness'
# log, the case's selftest output) are kept per run under
# ${REC_TEST_DIR:-/tmp/rec-screen-load}/load-<pid>/r<round>-<case>/.
#
# Prints one line per case run and a total; exits non-zero if any run failed.
set -eu
cd "$(dirname "$0")/../.."

rounds=${1:?usage: load.sh ROUNDS CASE...}
shift
[ "$#" -gt 0 ] || { echo "load.sh: name at least one case" >&2; exit 2; }
root="${REC_TEST_DIR:-/tmp/rec-screen-load}/load-$$"
mkdir -p "$root"

pids=""
stop_burners() {
    for p in $pids; do
        kill "$p" 2>/dev/null || true
    done
}
trap stop_burners EXIT INT TERM

burners=${LOAD_BURNERS:-8}
i=1
while [ "$i" -le "$burners" ]; do
    # `yes` and no disk: pure CPU, which is what starves a compositor's
    # frame clock and the portal's D-Bus replies.
    yes >/dev/null &
    pids="$pids $!"
    i=$((i + 1))
done
# Let the load settle before the first run.
sleep 2

pass=0
fail=0
round=1
while [ "$round" -le "$rounds" ]; do
    for case in "$@"; do
        dir="$root/r$round-$case"
        mkdir -p "$dir"
        if REC_TEST_DIR="$dir" scripts/selftest.sh "$case" >"$dir/selftest.out" 2>&1; then
            pass=$((pass + 1))
            echo "pass  r$round $case"
        else
            fail=$((fail + 1))
            echo "FAIL  r$round $case  (logs in $dir)"
        fi
    done
    round=$((round + 1))
done
echo "$pass passed, $fail failed (burners=$burners, logs in $root)"
[ "$fail" -eq 0 ]
