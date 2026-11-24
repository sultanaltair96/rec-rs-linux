#!/usr/bin/env bash
# Reusable fault fixtures (Bash, Linux /proc).
#
# Standalone smoke tests, inside the dev container:
#   scripts/dev.sh scripts/faults/session.sh scripts/faults/run.sh dock-output-first
#   scripts/dev.sh scripts/faults/session.sh scripts/faults/run.sh dock-input-first
#   scripts/dev.sh scripts/faults/session.sh scripts/faults/run.sh daemon-restart
#   scripts/dev.sh scripts/faults/session.sh scripts/faults/run.sh stop-cont
#   scripts/dev.sh scripts/faults/session.sh scripts/faults/run.sh camera-fallback
#   scripts/dev.sh scripts/faults/enospc.sh
#
# A custom driver can source common.sh under session.sh, install an EXIT trap
# calling fault_cleanup, then fault_audio_start and fault_spawn consumer CMD ...
# to interleave its own readiness/assertions with fault_* functions. Use
# fault_suspend consumer 2 to stop/resume ONLY that driver's owned child.
# run.sh optionally accepts a consumer command after the case; it is stopped
# at fixture teardown. No shell eval, arbitrary PID, pkill or systemctl used.
# Consumer logs are transient under REC_FAULT_ROOT; copy evidence elsewhere
# from your driver before exiting. Do not store recordings in the repository.
#
# Dock: private null sink + loopback mic, three rapid remove/restore cycles.
# Both removal and restoration use the named order. This is graph churn, NOT
# USB/udev unplug, device selection/fallback, or an assertion of app recovery.
# Restart: the entire owned PipeWire/Pulse/WirePlumber stack, not host services.
# Camera: GStreamer ball, YUY2 640x480@30, PipeWire Video/Source node named
# rec_fault_camera. Synthetic fallback for tests without a camera; NOT a V4L2
# device, portal grant, automatic application fallback, or decoded-frame test.
#
# ENOSPC requires unprivileged user + mount namespaces (exit 77 when blocked,
# e.g. Docker default seccomp). No sudo, host mounts, disk filling, or /dev/full
# substitution. A private 8 MiB tmpfs is filled and errno.ENOSPC verified.
# Optional command receives REC_FAULT_FULL_DIR, e.g.:
#   scripts/faults/enospc.sh bash -c 'rec-rs record --test-pattern --no-mic -t 2 -o "$REC_FAULT_FULL_DIR/clip.mp4"'
# That command's exit status is propagated, not interpreted as a recorder test.
# Namespace teardown releases the mount even if killed. tmpfs allocation is
# bounded to 8 MiB; metadata is additional. No application assertions are made.
#
# Safety: unique private XDG runtime/config/state/cache + D-Bus bus, no host
# PipeWire/Pulse sockets; WirePlumber physical monitors disabled. Children are
# direct children tracked in-memory and ownership checked in /proc before
# signalling. EXIT cleanup CONTs stopped children before TERM and bounded KILL.
# SIGKILL cannot run traps: use the dev container for hard-kill experiments.
# Commands you explicitly supply must themselves respect the private session;
# this wrapper is isolation for fixtures, not a security sandbox for commands.
