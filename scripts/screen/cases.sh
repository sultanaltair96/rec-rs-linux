#!/bin/sh
# The screen-capture self-test cases that need a live session.
#
#   scripts/screen/cases.sh portal   # inside scripts/screen/headless.sh (Wayland)
#   scripts/screen/cases.sh region   # inside scripts/screen/headless.sh
#   scripts/screen/cases.sh token    # inside scripts/screen/headless.sh
#   scripts/screen/cases.sh x11      # inside scripts/screen/xvfb.sh
#   scripts/screen/cases.sh window   # inside scripts/screen/xvfb.sh
#
# Each case records a real file and checks it itself; the caller (`selftest.sh`)
# then verifies the size, the frame rate and the motion. Exits non-zero on the
# first failed step, so the caller only has to look at the exit status.
#
# REC_BIN and OUT_DIR come from the caller's environment.
set -eu

B=${REC_BIN:?REC_BIN must point at the rec-rs binary}
OUT=${OUT_DIR:?OUT_DIR must be the self-test output directory}
case=${1:?usage: cases.sh portal|region|token|x11|window}

# A recording that must decode completely: `probe` (GStreamer decodes every
# sample) plus a full ffmpeg decode.
verify() {
    file=$1
    shift
    "$B" record -q "$@" -o "$file" >/dev/null 2>"$OUT/$(basename "$file").log"
    "$B" probe "$file" >>"$OUT/$(basename "$file").log" 2>&1
    ffmpeg -v error -i "$file" -f null - 2>>"$OUT/$(basename "$file").log"
}

case "$case" in
portal)
    # The Wayland pipeline: portal ScreenCast -> PipeWire -> writer. The
    # restore token must be stored 0600 for the next run to skip the picker.
    verify "$OUT/pattern-screen.mp4" -t 3 --no-mic --no-system-audio
    "$B" permissions >"$OUT/pattern-screen.permissions" 2>&1
    grep -q granted "$OUT/pattern-screen.permissions"
    token="$XDG_STATE_HOME/rec-rs/portal-token"
    test -s "$token"
    test "$(stat -c %a "$token")" = 600
    ;;
region)
    # --region crops the shared monitor: 640x480 native pixels out of a
    # 1280x720 stream, not a scaled 16:9 frame.
    verify "$OUT/screen-region.mp4" -t 2 --no-mic --no-system-audio --region 100,100,640,480
    ;;
token)
    # `permissions --request` runs the dialog once and stores the grant; a
    # corrupt token is ignored (with a message) instead of failing a recording.
    "$B" permissions --request >"$OUT/screen-token.request" 2>&1
    grep -q granted "$OUT/screen-token.request"
    token="$XDG_STATE_HOME/rec-rs/portal-token"
    test -s "$token"
    echo garbage >"$token"
    REC_PORTAL_TIMEOUT=30 "$B" record -q -t 1 --no-mic --no-system-audio \
        -o "$OUT/screen-token.mp4" 2>"$OUT/screen-token.log"
    grep -qi 'grant' "$OUT/screen-token.log"
    "$B" permissions >"$OUT/screen-token.permissions" 2>&1
    grep -q granted "$OUT/screen-token.permissions"
    ;;
x11)
    # The direct X11 path: monitor enumeration through RandR, then a capture of
    # the whole screen without a portal in sight.
    export REC_SCREEN_BACKEND=x11
    "$B" devices >"$OUT/screen-x11.devices" 2>&1
    grep -q '\[1\] id' "$OUT/screen-x11.devices"
    verify "$OUT/screen-x11.mp4" -t 2 --no-mic --no-system-audio
    ;;
window)
    # --window <app> captures a single X11 client's own surface (here a
    # GStreamer test pattern in a 400x300 window).
    export REC_SCREEN_BACKEND=x11
    verify "$OUT/screen-window.mp4" -t 2 --no-mic --no-system-audio --window gstreamer
    ;;
*)
    echo "cases.sh: unknown case $case" >&2
    exit 2
    ;;
esac
