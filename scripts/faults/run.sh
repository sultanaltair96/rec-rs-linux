#!/usr/bin/env bash
# Run under session.sh; optional command runs concurrently with the faults.
set -euo pipefail
source "$(dirname "$0")/common.sh"
trap fault_cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
case=${1:-}; [[ $# == 0 ]] || shift
case $case in
    dock-output-first|dock-input-first|daemon-restart|stop-cont|camera-fallback) ;;
    *) echo 'usage: run.sh {dock-output-first|dock-input-first|daemon-restart|stop-cont|camera-fallback} [COMMAND ARGS...]' >&2; exit 2;;
esac
fault_audio_start
if [[ $# -gt 0 ]]; then fault_spawn consumer "$@"; fi
sleep 1
case $case in
    dock-*)
        fault_dock_output_add; fault_dock_input_add
        sleep 1
        # Partial audio-only surrogate: not the screen/camera/sink dock burst.
        # Disconnect and restore in the selected order, repeated three times.
        for round in 1 2 3; do
            if [[ $case == dock-output-first ]]; then
                fault_dock_output_remove; fault_dock_input_remove
                sleep .2
                fault_dock_output_add; fault_dock_input_add
            else
                fault_dock_input_remove; fault_dock_output_remove
                sleep .2
                fault_dock_input_add; fault_dock_output_add
            fi
            sleep 1
            pw-dump | grep 'rec_fault_mic' >/dev/null
            pactl list short sinks | grep rec_fault_dock >/dev/null
            echo "PASS audio-only fixture $case round=$round (not the full dock fault)"
        done
        fault_dock_input_remove; fault_dock_output_remove
        ;;
    daemon-restart)
        fault_audio_stop
        sleep 1
        fault_audio_start
        echo 'PASS daemon-restart: private PipeWire/WirePlumber/Pulse stack ready'
        ;;
    stop-cont)
        fault_suspend pipewire "${FAULT_SUSPEND_SECS:-30}"
        fault_wait_pulse
        echo 'PASS stop-cont: private PipeWire resumed'
        ;;
    camera-fallback)
        fault_camera_start
        for ((i=0; i<100; i++)); do
            if pw-dump | grep rec_fault_camera >/dev/null; then break; fi
            sleep .1
        done
        pw-dump | grep rec_fault_camera >/dev/null
        fault_owned camera
        echo 'PASS camera-fallback: synthetic PipeWire camera advertised'
        ;;
esac
# Fixture success is not an assertion about the optional consumer's recovery.
sleep 1
