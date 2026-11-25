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
        # A stable fallback lets us verify default migration on every unplug.
        FAULT_BASE=$(pactl load-module module-null-sink sink_name=rec_fault_base)
        fault_dock_output_add; fault_dock_video_add
        # Output means default sink; input means the screen + camera producers.
        # Removal and restoration use both orders in the two matrix cases.
        for round in 1 2 3; do
            if [[ $case == dock-output-first ]]; then
                fault_dock_output_remove; fault_dock_video_remove
                [[ $(pactl get-default-sink) != rec_fault_dock ]]
                fault_node_wait rec_fault_dock absent
                sleep .2
                fault_dock_output_add; fault_dock_video_add
            else
                fault_dock_video_remove; fault_dock_output_remove
                [[ $(pactl get-default-sink) != rec_fault_dock ]]
                fault_node_wait rec_fault_dock absent
                sleep .2
                fault_dock_video_add; fault_dock_output_add
            fi
            sleep 1
            fault_node_wait rec_fault_dock present Audio/Sink
            [[ $(pactl get-default-sink) == rec_fault_dock ]]
            echo "PASS dock graph fixture $case round=$round (screen + camera Video/Source + default sink restored)"
        done
        fault_dock_video_remove; fault_dock_output_remove
        pactl unload-module "$FAULT_BASE"
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
        fault_node_wait rec_fault_camera present Video/Source
        fault_owned camera
        echo 'PASS camera-fallback: synthetic PipeWire camera advertised'
        ;;
esac
# Fixture success is not an assertion about the optional consumer's recovery.
sleep 1
