#!/bin/sh
# A private, headless PipeWire session for the phase 1B tests.
#
# Source this file (inside `dbus-run-session`, see scripts/audio/integration.sh)
# and call harness_start; harness_stop (also on exit) kills everything it
# started. Nothing on the host desktop is touched: the session gets its own
# XDG_RUNTIME_DIR, D-Bus session and graph.
#
# The graph it builds:
#
#   rec_test_sink        null sink, 48 kHz (the default output)
#   rec_test_sink_44k    null sink, 44.1 kHz (the resampling case)
#   rec_test_mic         a source fed by a loopback; play into
#   rec_test_mic_play    the loopback's playback side
#
# and a WAV generator (scripts/audio/mkwav.py) played with pw-play.
#
# Adapted from rec-rs scripts/selftest.sh (MIT OR Apache-2.0).

HARNESS_DIR="${REC_TEST_DIR:-${TMPDIR:-/tmp}}/pipewire-harness"
HARNESS_PIDS=""
HARNESS_STARTED=0

# Say one line: pass/fail with a reason. The integration script tallies them.
harness_log() { printf '%s\n' "$*"; }

_harness_wait_socket() {
    i=0
    while [ ! -S "$XDG_RUNTIME_DIR/pipewire-0" ]; do
        i=$((i + 1))
        if [ "$i" -gt 200 ]; then
            echo "harness: PipeWire did not start; log:" >&2
            tail -20 "$HARNESS_DIR/pipewire.log" >&2 || true
            return 1
        fi
        sleep 0.05
    done
}

_harness_wait_pulse() {
    i=0
    until [ -S "$XDG_RUNTIME_DIR/pulse/native" ] && pactl info >/dev/null 2>&1; do
        i=$((i + 1))
        if [ "$i" -gt 200 ]; then
            echo "harness: pipewire-pulse did not start; log:" >&2
            tail -20 "$HARNESS_DIR/pipewire-pulse.log" >&2 || true
            return 1
        fi
        sleep 0.05
    done
}

_harness_wait_node() {
    cls=$1; name=$2
    i=0
    until pw-dump 2>/dev/null | grep -q "\"node.name\": *\"$name\""; do
        i=$((i + 1))
        if [ "$i" -gt 100 ]; then
            echo "harness: $cls node $name did not appear" >&2
            return 1
        fi
        sleep 0.05
    done
}

harness_start() {
    [ "$HARNESS_STARTED" = 1 ] && return 0
    mkdir -p "$HARNESS_DIR"
    trap harness_stop EXIT INT TERM

    if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
        XDG_RUNTIME_DIR="$HARNESS_DIR/run"
        rm -rf "$XDG_RUNTIME_DIR"
        mkdir -p "$XDG_RUNTIME_DIR"
        chmod 700 "$XDG_RUNTIME_DIR"
        export XDG_RUNTIME_DIR
    fi
    # PipeWire looks here first; keep the socket beside the session's files.
    export PIPEWIRE_RUNTIME_DIR="$XDG_RUNTIME_DIR"
    # libpulse autospawns a server when its socket is not there yet; that
    # second pipewire-pulse would race the one started here for its directory.
    export PULSE_SERVER="unix:$XDG_RUNTIME_DIR/pulse/native"
    PULSE_CONF="$HARNESS_DIR/pulse-client.conf"
    printf 'autospawn = no\n' >"$PULSE_CONF"
    export PULSE_CLIENTCONFIG="$PULSE_CONF"

    pipewire >"$HARNESS_DIR/pipewire.log" 2>&1 &
    HARNESS_PIDS="$HARNESS_PIDS $!"
    _harness_wait_socket || return 1

    wireplumber >"$HARNESS_DIR/wireplumber.log" 2>&1 &
    HARNESS_PIDS="$HARNESS_PIDS $!"
    pipewire-pulse >"$HARNESS_DIR/pipewire-pulse.log" 2>&1 &
    HARNESS_PIDS="$HARNESS_PIDS $!"
    _harness_wait_pulse || return 1

    # A null sink at 48 kHz whose node.latency is known, so `devices` has a
    # latency to print that is not zero, and one at 44.1 kHz for resampling.
    pactl load-module module-null-sink sink_name=rec_test_sink rate=48000 \
        sink_properties="node.latency=256/48000" >/dev/null
    pactl load-module module-null-sink sink_name=rec_test_sink_44k rate=44100 >/dev/null

    # A virtual microphone: pw-loopback captures what is played into its
    # (fake) sink and plays it out of its (fake) source. The capture side is
    # the sink to play into, the playback side the source to record from.
    pw-loopback -m '[ FL FR ]' \
        --capture-props='media.class=Audio/Sink node.name=rec_test_mic_play node.description=Rec-Test-Mic-Play' \
        --playback-props='media.class=Audio/Source node.name=rec_test_mic node.description=Rec-Test-Mic' \
        >"$HARNESS_DIR/loopback.log" 2>&1 &
    HARNESS_LOOPBACK_PID=$!
    HARNESS_PIDS="$HARNESS_PIDS $!"

    _harness_wait_node Sink rec_test_sink || return 1
    _harness_wait_node Source rec_test_mic || return 1
    _harness_wait_node Sink rec_test_mic_play || return 1

    # Defaults: the test sink and the virtual mic, as a desktop would have.
    pactl set-default-sink rec_test_sink >/dev/null
    pactl set-default-source rec_test_mic >/dev/null

    HARNESS_STARTED=1
    harness_log "harness: PipeWire up (XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR)"
}

harness_stop() {
    [ "$HARNESS_STARTED" = 1 ] || return 0
    HARNESS_STARTED=0
    trap - EXIT INT TERM
    # shellcheck disable=SC2086 # the list is word-split on purpose
    [ -n "$HARNESS_PIDS" ] && kill $HARNESS_PIDS 2>/dev/null || true
    # shellcheck disable=SC2086
    wait $HARNESS_PIDS 2>/dev/null || true
    HARNESS_PIDS=""
}

# play_wav NODE FILE [extra pw-play args...]: play in the background; sets
# PLAY_PID so a case can wait for it.
play_wav() {
    node=$1; file=$2
    shift 2
    pw-play --target "$node" "$@" "$file" >"$HARNESS_DIR/play-$node.log" 2>&1 &
    PLAY_PID=$!
}

# wait_play: wait for the background player, ignoring its status (a killed
# player in the unplug case is expected).
wait_play() {
    [ -n "${PLAY_PID:-}" ] && wait "$PLAY_PID" 2>/dev/null || true
    PLAY_PID=""
}

# mkwav FILE args...: generate a test WAV with the helper.
mkwav() {
    file=$1
    shift
    python3 scripts/audio/mkwav.py "$file" "$@"
}

# default_sink NAME: switch the default output (the metadata change the source
# should follow on the next recording).
default_sink() {
    pactl set-default-sink "$1" >/dev/null
}
