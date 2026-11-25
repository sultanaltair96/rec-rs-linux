#!/bin/sh
# Real GNOME Shell/Mutter + GNOME ScreenCast portal, software rendered.
# Usage: scripts/dev.sh scripts/screen/gnome-headless.sh <command> [args...]
# This deliberately does NOT auto-accept, replace, or fake the GNOME picker.
# A capture request needs real user interaction (or a previously granted token).
# Readiness only proves services are available, never that capture succeeded.
set -eu
if [ "${REC_GNOME_SESSION:-}" != 1 ]; then
    exec dbus-run-session -- env REC_GNOME_SESSION=1 sh "$0" "$@"
fi
[ "$#" -gt 0 ] || { echo "usage: $0 command [args...]" >&2; exit 2; }
BASE=${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}
mkdir -p "$BASE"
TMP=$(mktemp -d "$BASE/gnome.XXXXXX")
LOG="$TMP/log"
mkdir -p "$LOG" "$TMP/run" "$TMP/config/xdg-desktop-portal" "$TMP/cache" "$TMP/state" "$TMP/data"
chmod 700 "$TMP/run"
pids=""
cleanup() {
    status=$?
    trap - EXIT INT TERM
    for pid in $pids; do kill "$pid" 2>/dev/null || true; done
    for pid in $pids; do wait "$pid" 2>/dev/null || true; done
    echo "gnome-headless: logs retained in $LOG" >&2
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
export XDG_RUNTIME_DIR="$TMP/run" XDG_CONFIG_HOME="$TMP/config"
export XDG_CACHE_HOME="$TMP/cache" XDG_STATE_HOME="$TMP/state" XDG_DATA_HOME="$TMP/data"
export XDG_CURRENT_DESKTOP=GNOME XDG_SESSION_DESKTOP=gnome XDG_SESSION_TYPE=wayland
export GSETTINGS_BACKEND=memory LIBGL_ALWAYS_SOFTWARE=1
unset DISPLAY WAYLAND_DISPLAY SWAYSOCK
# Shell requires a reachable system bus even without logind. This is a real
# private bus, not a mock login/portal service and never the host system bus.
export DBUS_SYSTEM_BUS_ADDRESS="unix:path=$TMP/system-bus"
dbus-daemon --session --nofork --address="$DBUS_SYSTEM_BUS_ADDRESS" >"$LOG/system-bus.log" 2>&1 &
pids="$pids $!"
i=0
while [ ! -S "$TMP/system-bus" ]; do
    i=$((i + 1)); [ "$i" -lt 100 ] || exit 1; sleep 0.1
done
# Do not fall back to wlr, whose chooser semantics differ from GNOME's.
printf '[preferred]\ndefault=gnome\norg.freedesktop.impl.portal.ScreenCast=gnome\norg.freedesktop.impl.portal.RemoteDesktop=gnome\n' >"$XDG_CONFIG_HOME/xdg-desktop-portal/portals.conf"
pipewire >"$LOG/pipewire.log" 2>&1 &
pids="$pids $!"
wireplumber >"$LOG/wireplumber.log" 2>&1 &
pids="$pids $!"
gnome-shell --headless --wayland --virtual-monitor "${SCREEN_WIDTH:-1920}x${SCREEN_HEIGHT:-1080}" >"$LOG/gnome-shell.log" 2>&1 &
shell_pid=$!
pids="$pids $shell_pid"
i=0
while [ "$i" -lt 150 ]; do
    kill -0 "$shell_pid" 2>/dev/null || { tail -60 "$LOG/gnome-shell.log" >&2; exit 1; }
    for sock in "$XDG_RUNTIME_DIR"/wayland-*; do
        case "$sock" in *.lock) continue ;; esac
        if [ -S "$sock" ]; then export WAYLAND_DISPLAY="${sock##*/}"; break; fi
    done
    if [ -n "${WAYLAND_DISPLAY:-}" ] && [ -S "$XDG_RUNTIME_DIR/pipewire-0" ]; then break; fi
    i=$((i + 1)); sleep 0.1
done
[ -n "${WAYLAND_DISPLAY:-}" ] && [ -S "$XDG_RUNTIME_DIR/pipewire-0" ] || {
    echo "gnome-headless: compositor/PipeWire startup timed out" >&2; exit 1;
}
# D-Bus activation must see this private compositor rather than a host display.
dbus-update-activation-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR XDG_CURRENT_DESKTOP XDG_SESSION_TYPE XDG_CONFIG_HOME XDG_DATA_HOME GSETTINGS_BACKEND DBUS_SYSTEM_BUS_ADDRESS
/usr/libexec/xdg-desktop-portal-gnome >"$LOG/portal-gnome.log" 2>&1 &
pids="$pids $!"
/usr/libexec/xdg-desktop-portal --verbose >"$LOG/portal.log" 2>&1 &
pids="$pids $!"
i=0
while ! gdbus call --session --dest org.freedesktop.portal.Desktop \
    --object-path /org/freedesktop/portal/desktop \
    --method org.freedesktop.DBus.Properties.Get org.freedesktop.portal.ScreenCast version \
    >"$LOG/screencast-version.log" 2>&1; do
    i=$((i + 1))
    [ "$i" -lt 100 ] || { echo "gnome-headless: ScreenCast unavailable (see $LOG)" >&2; exit 1; }
    sleep 0.1
done
gdbus call --session --dest org.freedesktop.portal.Desktop \
    --object-path /org/freedesktop/portal/desktop \
    --method org.freedesktop.DBus.Properties.Get org.freedesktop.portal.ScreenCast AvailableSourceTypes \
    >"$LOG/source-types.log"
if grep -q 'uint32 0>' "$LOG/source-types.log"; then
    echo "gnome-headless: portal advertises no capture sources" >&2
    exit 1
fi
kill -0 "$shell_pid" 2>/dev/null || {
    echo "gnome-headless: Shell exited during portal startup" >&2
    tail -60 "$LOG/gnome-shell.log" >&2
    exit 1
}
# Expose logs for diagnostic commands; none of this implies picker approval.
export SCREEN_GNOME_LOG_DIR="$LOG"
echo "gnome-headless: real GNOME portal ready on $WAYLAND_DISPLAY; picker requires user consent; logs $LOG" >&2
"$@"
