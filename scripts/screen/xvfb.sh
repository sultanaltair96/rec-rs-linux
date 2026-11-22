#!/bin/sh
# Run a command against an X11 server with no display, for the X11 tests.
#
#   scripts/screen/xvfb.sh <command...>
#
# Xvfb gives a real X server (RandR, XFixes, MIT-SHM) with a known screen size,
# which is what the direct X11 backend needs: monitor enumeration, window
# capture and the pointer all work without a GPU. There is no window manager,
# so `_NET_CLIENT_LIST` is absent — the backend's fallback (mapped top-level
# windows with a name) is what finds a window here, which is exactly the code
# path a bare X session uses.
set -eu

TMP=$(mktemp -d "${TMPDIR:-/tmp}/rec-screen-xvfb.XXXXXX")
LOG="$TMP/log"
mkdir -p "$LOG"
DISPLAY_NUM=${XVFB_DISPLAY:-99}
WIDTH=${SCREEN_WIDTH:-1280}
HEIGHT=${SCREEN_HEIGHT:-800}

pids=""
stop() {
    status=$?
    for pid in $pids; do
        kill "$pid" 2>/dev/null || true
    done
    for pid in $pids; do
        wait "$pid" 2>/dev/null || true
    done
    echo "xvfb: session ended (logs in $LOG)" >&2
    exit "$status"
}
trap stop EXIT INT TERM

Xvfb ":$DISPLAY_NUM" -screen 0 "${WIDTH}x${HEIGHT}x24" -nolisten tcp +extension RANDR \
    >"$LOG/xvfb.log" 2>&1 &
pids="$pids $!"
export DISPLAY=":$DISPLAY_NUM"
# Wait for the server to accept connections: `xdpyinfo` is not installed, so
# probe with a tiny client (our own binary lists displays through RandR).
i=0
while [ $i -lt 100 ]; do
    if REC_SCREEN_BACKEND=x11 "$REC_BIN" devices >"$LOG/devices.log" 2>&1; then
        break
    fi
    i=$((i + 1))
    sleep 0.1
done

# --- something that moves ---------------------------------------------------
# ximagesink draws into its own X window, which gives the direct X11 backend a
# window to capture and the root window something that changes.
if [ -z "${SCREEN_NO_ANIMATION:-}" ]; then
    gst-launch-1.0 -q videotestsrc is-live=true pattern=ball \
        ! video/x-raw,width=400,height=300,framerate=30/1 \
        ! ximagesink window-width=400 window-height=300 sync=false \
        >"$LOG/animation.log" 2>&1 &
    pids="$pids $!"
    sleep 1.5
fi

echo "xvfb: $DISPLAY ${WIDTH}x${HEIGHT}, logs in $LOG" >&2
"$@"
