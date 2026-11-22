#!/bin/sh
# Run a command inside a headless desktop, for the screen-capture self-tests.
#
#   scripts/screen/headless.sh <command...>
#   scripts/screen/headless.sh --keep <command...>   # leave the session up
#
# Inside there is a private D-Bus session, PipeWire + WirePlumber, sway on the
# headless wlroots backend with one 1280x720 output, xdg-desktop-portal with
# xdg-desktop-portal-wlr (configured to share that output without a chooser),
# and a GStreamer test pattern drawing into the output through waylandsink so
# the captured frames actually change.
#
# The environment it exports (WAYLAND_DISPLAY, DBUS_SESSION_BUS_ADDRESS,
# XDG_RUNTIME_DIR, XDG_CURRENT_DESKTOP, XDG_STATE_HOME) is what makes
# `rec-rs record` take the portal path:
#
#   scripts/dev.sh scripts/screen/headless.sh \
#       '$CARGO_TARGET_DIR/release/rec-rs record -t 3 -o /tmp/screen.mp4 --no-mic'
#
# Test output goes to $REC_TEST_DIR (default /tmp), never into the repo.
set -eu

# The portal talks D-Bus, so the whole session lives inside a private session
# bus: no real desktop's bus is touched, and everything dies with this script.
if [ "${REC_HEADLESS_SESSION:-}" != "1" ]; then
    exec dbus-run-session -- env REC_HEADLESS_SESSION=1 sh "$0" "$@"
fi

# --- paths and cleanup ------------------------------------------------------
TMP=$(mktemp -d "${TMPDIR:-/tmp}/rec-screen-headless.XXXXXX")
RUNTIME="$TMP/run"
STATE="$TMP/state"
CONFIG="$TMP/config"
LOG="$TMP/log"
mkdir -p "$RUNTIME" "$STATE" "$CONFIG" "$LOG" "$CONFIG/xdg-desktop-portal-wlr"
chmod 700 "$RUNTIME"

pids=""
stop() {
    status=$?
    for pid in $pids; do
        kill "$pid" 2>/dev/null || true
    done
    for pid in $pids; do
        wait "$pid" 2>/dev/null || true
    done
    echo "headless: session ended (logs in $LOG)" >&2
    exit "$status"
}
trap stop EXIT INT TERM

# --- session environment ----------------------------------------------------
export XDG_RUNTIME_DIR="$RUNTIME"
export XDG_STATE_HOME="$STATE"
export XDG_CONFIG_HOME="$CONFIG"
export XDG_CURRENT_DESKTOP=sway
export XDG_SESSION_TYPE=wayland
export WLR_BACKENDS=headless
export WLR_LIBINPUT_NO_DEVICES=1
# No GPU in the container: the pixman renderer draws into SHM.
export WLR_RENDERER=pixman
export WLR_HEADLESS_OUTPUTS=1
export WLR_NO_HARDWARE_CURSORS=1
# Do not inherit a real desktop's sockets.
unset DISPLAY WAYLAND_DISPLAY SWAYSOCK

OUTPUT_WIDTH=${SCREEN_WIDTH:-1280}
OUTPUT_HEIGHT=${SCREEN_HEIGHT:-720}

# --- sway -------------------------------------------------------------------
# One named output at a known size: the 1080p default then asks for exactly the
# native size (1280x720 fits inside 1920x1080), so no scaling is involved.
cat >"$CONFIG/sway.conf" <<EOF
output HEADLESS-1 resolution ${OUTPUT_WIDTH}x${OUTPUT_HEIGHT}
default_border none
EOF
sway --unsupported-gpu -c "$CONFIG/sway.conf" >"$LOG/sway.log" 2>&1 &
pids="$pids $!"
# The socket is named wayland-<n>; find it rather than guess the number.
i=0
while [ $i -lt 100 ]; do
    for sock in "$RUNTIME"/wayland-*; do
        case "$sock" in
            *.lock) continue ;;
        esac
        if [ -S "$sock" ]; then
            export WAYLAND_DISPLAY=$(basename "$sock")
            break 2
        fi
    done
    i=$((i + 1))
    sleep 0.1
done
if [ -z "${WAYLAND_DISPLAY:-}" ]; then
    echo "headless: sway did not create a Wayland socket" >&2
    cat "$LOG/sway.log" >&2
    exit 1
fi
export SWAYSOCK=$(ls "$RUNTIME"/sway-ipc.* 2>/dev/null | head -1)

# --- PipeWire ---------------------------------------------------------------
# The portal's ScreenCast hands out a node on the session PipeWire, so a daemon
# (and a session manager, for the policy) has to be there.
pipewire >"$LOG/pipewire.log" 2>&1 &
pids="$pids $!"
wireplumber >"$LOG/wireplumber.log" 2>&1 &
pids="$pids $!"
i=0
while [ $i -lt 100 ] && [ ! -S "$RUNTIME/pipewire-0" ]; do
    i=$((i + 1))
    sleep 0.1
done
if [ ! -S "$RUNTIME/pipewire-0" ]; then
    echo "headless: PipeWire did not start" >&2
    cat "$LOG/pipewire.log" >&2
    exit 1
fi

# --- portal -----------------------------------------------------------------
# output_name pins the screencast to our headless output, so no chooser (dmenu,
# rofi or a dialog) is ever involved: the picker answers instantly.
# wlr always asks a chooser which output to share. There is no dialog to click
# headless, so the chooser is a two-line script: it reads the options and picks
# the headless output. (Its "whitelisted" choosers are dmenu-likes, which would
# need a terminal or a bar.)
cat >"$CONFIG/choose.sh" <<'EOF'
#!/bin/sh
# stdin: one "Monitor: NAME" line per output. Answer with the headless one.
while IFS= read -r line; do
    case "$line" in
        *HEADLESS*) printf '%s\n' "$line"; exit 0 ;;
    esac
done
exit 1
EOF
chmod +x "$CONFIG/choose.sh"
cat >"$CONFIG/xdg-desktop-portal-wlr/config" <<EOF
[screencast]
output_name=HEADLESS-1
max_fps=60
chooser_type=dmenu
chooser_cmd=$CONFIG/choose.sh
EOF
/usr/libexec/xdg-desktop-portal-wlr -l DEBUG -r >"$LOG/portal-wlr.log" 2>&1 &
pids="$pids $!"
/usr/libexec/xdg-desktop-portal -r -v >"$LOG/portal.log" 2>&1 &
pids="$pids $!"
# The portal frontend is ready when it owns its bus name; ask it for the
# ScreenCast version (a call that never shows a dialog) a few times.
i=0
ready=""
while [ $i -lt 100 ]; do
    if command -v gdbus >/dev/null 2>&1 &&
        gdbus call --session --dest org.freedesktop.portal.Desktop \
            --object-path /org/freedesktop/portal/desktop \
            --method org.freedesktop.DBus.Properties.Get \
            org.freedesktop.portal.ScreenCast version >/dev/null 2>&1; then
        ready=1
        break
    fi
    i=$((i + 1))
    sleep 0.1
done
if [ -z "$ready" ]; then
    echo "headless: xdg-desktop-portal did not come up" >&2
    tail -20 "$LOG/portal.log" >&2
    cat "$LOG/portal-wlr.log" >&2
    exit 1
fi

# --- something that moves ---------------------------------------------------
# A terminal printing a counter as fast as it can: every line it scrolls is a
# new frame for the compositor, so the capture is provably not a still. foot
# draws through wl_shm, which works with the pixman renderer (waylandsink needs
# dmabuf, and there is no GPU here).
if [ -z "${SCREEN_NO_ANIMATION:-}" ]; then
    foot -T rec-test -e sh -c 'i=0; while :; do i=$((i+1)); printf "frame %s\n" "$i"; done' \
        >"$LOG/animation.log" 2>&1 &
    pids="$pids $!"
    sleep 1.5
fi

echo "headless: $WAYLAND_DISPLAY ${OUTPUT_WIDTH}x${OUTPUT_HEIGHT}, portal ready, logs in $LOG" >&2
# Run the command with the session in place. `set -a` is not needed: the
# variables above are already exported.
"$@"
