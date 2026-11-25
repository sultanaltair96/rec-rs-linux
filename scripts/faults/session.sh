#!/usr/bin/env bash
# Private session wrapper. Usage: session.sh COMMAND [ARG...]
set -euo pipefail
if [[ ${1:-} != --inside ]]; then
    [[ $# -gt 0 ]] || { echo 'usage: session.sh COMMAND [ARG...]' >&2; exit 2; }
    root=$(mktemp -d "${TMPDIR:-/tmp}/rec-faults.XXXXXXXX")
    trap 'rm -rf -- "$root"' EXIT
    mkdir -m 700 "$root/run" "$root/config" "$root/state" "$root/cache"
    # Do not enumerate/control physical devices, even on a developer desktop.
    mkdir -p "$root/config/wireplumber/wireplumber.conf.d"
    printf '%s\n' 'wireplumber.profiles = { main = { monitor.alsa = disabled monitor.bluez = disabled monitor.v4l2 = disabled monitor.libcamera = disabled } }' \
        >"$root/config/wireplumber/wireplumber.conf.d/90-faults.conf"
    export GIO_USE_VFS=local
    export REC_FAULT_ROOT=$root XDG_RUNTIME_DIR=$root/run
    export XDG_CONFIG_HOME=$root/config XDG_STATE_HOME=$root/state XDG_CACHE_HOME=$root/cache
    export PIPEWIRE_RUNTIME_DIR=$root/run PIPEWIRE_REMOTE=pipewire-0
    export PULSE_SERVER=unix:$root/run/pulse/native
    unset DBUS_SESSION_BUS_ADDRESS DBUS_SESSION_BUS_PID
    unset PIPEWIRE_NODE PIPEWIRE_PROPS PIPEWIRE_CONFIG_DIR PIPEWIRE_CONFIG_NAME
    unset PULSE_CLIENTCONFIG PULSE_SINK PULSE_SOURCE
    dbus-run-session -- "$0" --inside "$@"
    exit $?
fi
shift
export REC_FAULT_SESSION=$$
printf '%s\n' "$REC_FAULT_SESSION" >"$REC_FAULT_ROOT/owner"
exec "$@"
