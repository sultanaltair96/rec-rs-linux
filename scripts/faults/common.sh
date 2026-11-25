#!/usr/bin/env bash
# Source from a test driver launched by session.sh. Never targets existing PIDs.
[[ -n ${BASH_VERSION:-} ]] || return 2
fault_check() {
    [[ -n ${REC_FAULT_ROOT:-} && -n ${REC_FAULT_SESSION:-} &&
       $XDG_RUNTIME_DIR == "$REC_FAULT_ROOT/run" &&
       $PIPEWIRE_RUNTIME_DIR == "$XDG_RUNTIME_DIR" &&
       $PULSE_SERVER == "unix:$XDG_RUNTIME_DIR/pulse/native" &&
       $(<"$REC_FAULT_ROOT/owner") == "$REC_FAULT_SESSION" ]] || {
        echo 'faults: refusing non-isolated session; use session.sh' >&2; return 2;
    }
}
fault_check || return
# Only direct children started here are signalled; never accept arbitrary PIDs.
declare -A FAULT_CHILDREN=()
fault_spawn() {
    local name=$1; shift
    [[ ! -v FAULT_CHILDREN[$name] ]] || return 2
    "$@" >"$REC_FAULT_ROOT/$name.log" 2>&1 &
    FAULT_CHILDREN[$name]=$!
}
fault_owned() {
    local pid=${FAULT_CHILDREN[$1]:-}
    [[ -n $pid && -r /proc/$pid/status ]] &&
        grep -Eq "^PPid:[[:space:]]+$BASHPID$" "/proc/$pid/status"
}
fault_stop() {
    local name=$1 pid=${FAULT_CHILDREN[$1]:-}
    if fault_owned "$name"; then
        kill -CONT "$pid" 2>/dev/null || true
        kill -TERM "$pid" 2>/dev/null || true
        # Bounded teardown, including a daemon which fails to handle TERM.
        for ((i=0; i<40; i++)); do
            kill -0 "$pid" 2>/dev/null || break
            sleep .05
        done
        kill -KILL "$pid" 2>/dev/null || true
    fi
    [[ -z $pid ]] || wait "$pid" 2>/dev/null || true
    unset 'FAULT_CHILDREN[$name]'
}
fault_cleanup() {
    local name
    for name in "${!FAULT_CHILDREN[@]}"; do fault_stop "$name"; done
}
fault_wait_pulse() {
    local i
    for ((i=0; i<100; i++)); do
        pactl info >/dev/null 2>&1 && return 0
        sleep .1
    done
    echo 'faults: Pulse readiness timeout (logs in private session)' >&2
    return 1
}
fault_audio_start() {
    fault_spawn pipewire pipewire
    local i
    for ((i=0; i<100; i++)); do
        [[ -S $XDG_RUNTIME_DIR/pipewire-0 ]] && break
        sleep .1
    done
    [[ -S $XDG_RUNTIME_DIR/pipewire-0 ]] || return 1
    fault_spawn wireplumber wireplumber
    fault_spawn pulse pipewire-pulse
    fault_wait_pulse
}
fault_audio_stop() {
    fault_stop pulse; fault_stop wireplumber; fault_stop pipewire
}
# Surrogate dock devices, not physical hotplug. No hardware or default changes.
fault_dock_output_add() {
    FAULT_SINK=$(pactl load-module module-null-sink sink_name=rec_fault_dock)
}
fault_dock_input_add() {
    fault_spawn dock-mic pw-loopback \
        --capture-props='media.class=Audio/Sink node.name=rec_fault_mic_feed' \
        --playback-props='media.class=Audio/Source node.name=rec_fault_mic'
}
fault_dock_output_remove() { pactl unload-module "${FAULT_SINK:?}"; unset FAULT_SINK; }
fault_dock_input_remove() { fault_stop dock-mic; }
# Synthetic camera source without v4l2loopback/root. Consumers need PipeWire support.
fault_camera_start() {
    fault_spawn camera gst-launch-1.0 -q videotestsrc is-live=true pattern=ball \
        '!' video/x-raw,format=YUY2,width=640,height=480,framerate=30/1 \
        '!' pipewiresink mode=provide \
        'stream-properties=props,node.name=rec_fault_camera,media.class=Video/Source'
}
fault_suspend() {
    local name=$1 seconds=${2:-1}
    [[ $seconds =~ ^[0-9]+$ && $seconds -le 60 ]] || return 2
    fault_owned "$name" || return 2
    kill -STOP "${FAULT_CHILDREN[$name]}"
    sleep "$seconds"
    fault_owned "$name" && kill -CONT "${FAULT_CHILDREN[$name]}"
}
