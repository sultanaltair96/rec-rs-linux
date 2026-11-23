#!/bin/sh
# Headless integration test for the phase 1B audio sources.
#
# Starts a private PipeWire session (scripts/audio/harness.sh) inside its own
# D-Bus session, plays a known tone/beep train into the test sink and the
# virtual microphone, records short test-pattern files with `rec-rs record` and
# checks what arrived with scripts/audio/tonecheck.py and ffprobe.
#
# Usage: scripts/audio/integration.sh [devices|system|resample|mic|missing|unplug|all]
#        (run it through the dev container: scripts/dev.sh scripts/audio/integration.sh)
#
# REC must point at a built rec-rs; the default is $CARGO_TARGET_DIR/release/rec-rs,
# then debug, then `target/{release,debug}`. Recordings go to
# ${REC_TEST_DIR:-/tmp}/pipewire-harness, never into the repo.
#
# Cases that need hardware are not here: this host has no camera (PARITY.md has
# the real-desktop checklist) and no physical sound card.
set -eu
cd "$(dirname "$0")/../.."

# WirePlumber and PipeWire want a session bus; give the test its own so it
# never talks to (or is disturbed by) a desktop.
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    exec dbus-run-session -- "$0" "$@"
fi

CASE="${1:-all}"

find_rec() {
    if [ -n "${REC:-}" ]; then
        printf '%s' "$REC"
        return
    fi
    for p in "${CARGO_TARGET_DIR:-target}/release/rec-rs" \
             "${CARGO_TARGET_DIR:-target}/debug/rec-rs" \
             target/release/rec-rs target/debug/rec-rs; do
        [ -x "$p" ] && { printf '%s' "$p"; return; }
    done
    echo "integration: no rec-rs binary; run scripts/dev.sh cargo build" >&2
    exit 1
}
REC=$(find_rec)

OUT="${REC_TEST_DIR:-${TMPDIR:-/tmp}}/pipewire-harness"
rm -rf "$OUT/clips"
mkdir -p "$OUT/clips"
CLIPS="$OUT/clips"

# shellcheck source=scripts/audio/harness.sh
. scripts/audio/harness.sh

pass=0
fail=0
skipped=0
ok() { echo "PASS  $1${2:+  ($2)}"; pass=$((pass + 1)); }
bad() { echo "FAIL  $1  ($2; see $3)"; fail=$((fail + 1)); }
skip() { echo "SKIP  $1  ($2)"; skipped=$((skipped + 1)); }
wanted() {
    [ "$CASE" = all ] || [ "$CASE" = "$1" ]
}

# The player starts first, so a slow capture start under load still records a
# beep train; a beep once a second lands 3-4 times in 5 s.
record_with_play() { # record_with_play NAME TARGET WAV ARGS...
    name=$1; target=$2; wav=$3
    shift 3
    f="$CLIPS/$name.mov"
    # A short delay lets the recorder open the graph first, so the file starts
    # at the beginning of the train. The WAVs are long enough (12 s) that a
    # slow start under CPU load still records several beeps.
    ( sleep 0.5; exec pw-play --target "$target" "$wav" ) \
        >"$OUT/$name.play.log" 2>&1 &
    player=$!
    status=0
    timeout "${REC_TIMEOUT:-90}" "$REC" record -q -o "$f" "$@" \
        >"$OUT/$name.out" 2>"$OUT/$name.log" || status=$?
    # The WAVs are longer than the clips (a slow start under load must still
    # catch beeps); stop the player instead of waiting for the train.
    kill "$player" 2>/dev/null || true
    wait "$player" 2>/dev/null || true
    return "$status"
}

ffprobe_audio_rate() {
    ffprobe -v error -select_streams a:0 -show_entries stream=sample_rate \
        -of csv=p=0 "$1" 2>/dev/null || true
}

harness_start || {
    echo "harness: could not start a PipeWire session" >&2
    exit 1
}

if wanted devices; then
    if "$REC" devices >"$OUT/devices.txt" 2>"$OUT/devices.err"; then
        if grep -q 'Rec-Test-Mic' "$OUT/devices.txt" \
           && grep -q 'rec_test_mic' "$OUT/devices.txt" \
           && grep -Eq 'Output latency \(system audio is shifted by this\): [0-9]+\.[0-9] ms' "$OUT/devices.txt"; then
            ok devices "$(grep -m1 'Output latency' "$OUT/devices.txt")"
        else
            bad devices "the mic or the output latency is missing" "$OUT/devices.txt"
        fi
    else
        bad devices "rec-rs devices failed" "$OUT/devices.err"
    fi
fi

# System audio at the sink's own 48 kHz.
if wanted system; then
    mkwav "$OUT/beeps-48k.wav" --rate 48000 --secs 12 --level -6 --beep 0.04 >/dev/null
    default_sink rec_test_sink
    name=system-48k
    if ( export REC_PATTERN_SYSTEM_AUDIO=pipewire REC_DEBUG=1; \
         record_with_play "$name" rec_test_sink "$OUT/beeps-48k.wav" \
           --test-pattern --no-mic -t 5 --system-audio-offset-ms 0 ); then
        if grep -q 'system format 48000 Hz' "$OUT/$name.log" \
           && python3 scripts/audio/tonecheck.py "$CLIPS/$name.mov" --beeps \
                  --expect-db -6 --audio 0 --max-gap 1.6 >"$OUT/$name.check" 2>&1; then
            ok "$name" "$(cat "$OUT/$name.check")"
        else
            bad "$name" "the tone/beeps were wrong" "$OUT/$name.check"
        fi
    else
        bad "$name" "recording failed" "$OUT/$name.log"
    fi
fi

# System audio from a 44.1 kHz sink: the source must capture 44.1 kHz (the log
# says so with REC_DEBUG) and the mixer must resample it without a pitch shift.
if wanted resample; then
    mkwav "$OUT/beeps-44k.wav" --rate 44100 --secs 12 --level -6 --beep 0.04 >/dev/null
    default_sink rec_test_sink_44k
    name=system-44k
    ( export REC_PATTERN_SYSTEM_AUDIO=pipewire REC_DEBUG=1; \
      record_with_play "$name" rec_test_sink_44k "$OUT/beeps-44k.wav" \
        --test-pattern --no-mic -t 5 --system-audio-offset-ms 0 ) || true
    # record_with_play's status is hidden when the function is in a condition;
    # the file and the log are the proof.
    if [ -s "$CLIPS/$name.mov" ] \
       && grep -q 'system format 44100 Hz' "$OUT/$name.log" \
       && python3 scripts/audio/tonecheck.py "$CLIPS/$name.mov" --beeps --audio 0 \
              --max-gap 1.6 >"$OUT/$name.check" 2>&1; then
        ok "$name" "captured 44100 Hz, resampled to the file's 48 kHz: $(cat "$OUT/$name.check")"
    else
        bad "$name" "44.1 kHz capture or resampling failed" "$OUT/$name.log"
    fi
    default_sink rec_test_sink
fi

# The default microphone (the harness sets rec_test_mic as the default source).
if wanted mic; then
    mkwav "$OUT/beeps-48k.wav" --rate 48000 --secs 12 --level -6 --beep 0.04 >/dev/null
    name=mic-default
    if REC_DEBUG=1 record_with_play "$name" rec_test_mic_play "$OUT/beeps-48k.wav" \
           --test-pattern --no-system-audio -t 4; then
        if grep -q 'mic format 48000 Hz' "$OUT/$name.log" \
           && python3 scripts/audio/tonecheck.py "$CLIPS/$name.mov" --beeps --audio 0 \
                  >"$OUT/$name.check" 2>&1; then
            ok "$name" "$(cat "$OUT/$name.check")"
        else
            bad "$name" "the virtual mic was not captured" "$OUT/$name.log"
        fi
    else
        bad "$name" "recording failed" "$OUT/$name.log"
    fi

    # An explicit device, by name substring (`devices` prints the id; the
    # description is what --mic also accepts).
    name=mic-by-name
    if record_with_play "$name" rec_test_mic_play "$OUT/beeps-48k.wav" \
           --test-pattern --no-system-audio --mic 'Rec-Test' -t 3 \
       && python3 scripts/audio/tonecheck.py "$CLIPS/$name.mov" --beeps --audio 0 \
              >"$OUT/$name.check" 2>&1; then
        ok "$name" "--mic 'Rec-Test': $(cat "$OUT/$name.check")"
    else
        bad "$name" "--mic by name substring failed" "$OUT/$name.log"
    fi

    # A requested mic that is not connected fails the session (the default mic
    # is the only one allowed to warn and continue).
    name=mic-missing
    status=0
    timeout 60 "$REC" record -q --test-pattern --no-system-audio \
        --mic definitely-not-a-microphone -t 2 -o "$CLIPS/$name.mov" \
        >"$OUT/$name.out" 2>"$OUT/$name.log" || status=$?
    if [ "$status" -ne 0 ] && grep -q 'is not connected' "$OUT/$name.log"; then
        ok "$name" "an explicit missing --mic fails with a clear message"
    else
        bad "$name" "a missing --mic did not fail (status $status)" "$OUT/$name.log"
    fi
fi

# Nothing to record: no PipeWire session at all must be a clear error, not a
# hang. Run it in a fresh environment with the socket path pointing nowhere.
if wanted missing; then
    name=no-pipewire
    status=0
    timeout 20 env XDG_RUNTIME_DIR=/nonexistent PIPEWIRE_RUNTIME_DIR=/nonexistent \
        "$REC" devices >"$OUT/$name.out" 2>"$OUT/$name.log" || status=$?
    if [ "$status" -eq 0 ] && grep -q 'unavailable' "$OUT/$name.out" \
       && ! grep -q 'Output latency' "$OUT/$name.out"; then
        ok "$name" "devices reports PipeWire as unavailable, no latency line, exit 0"
    else
        bad "$name" "devices did not report a missing session (status $status)" "$OUT/$name.out"
    fi
fi

# The audio hotplug watch 1E consumes: a new sink must notify within 5 s.
if wanted watch; then
    name=watch
    if cargo test --quiet the_device_watch_reports_a_new_sink -- --ignored --nocapture \
           >"$OUT/$name.check" 2>&1; then
        ok "$name" "$(grep -E 'test result' "$OUT/$name.check" | tail -1)"
    else
        bad "$name" "the PipeWire registry watch did not report the new sink" "$OUT/$name.check"
    fi
fi

# Under CPU load: the PipeWire mic must still deliver continuous, correctly
# stamped chunks (the G15 flakiness investigation). Opt-in because it saturates
# the container's CPUs.
if wanted stress; then
    name=stress
    burners=""
    for i in 1 2 3 4 5 6 7 8; do (while :; do :; done) & burners="$burners $!"; done
    okc=0
    cargo test --quiet live_mic_chunks_do_not_overlap -- --ignored --nocapture \
        >"$OUT/$name.continuity" 2>&1 && \
    cargo test --quiet live_chunk_timestamps_are_on_the_host_clock -- --ignored --nocapture \
        >"$OUT/$name.clock" 2>&1 && okc=1
    kill $burners 2>/dev/null || true
    wait $burners 2>/dev/null || true
    if [ "$okc" = 1 ]; then
        ok "$name" "$(grep -h -E '^mic continuity|^clock' "$OUT/$name.continuity" "$OUT/$name.clock" | paste -sd ';' -)"
    else
        bad "$name" "the mic overlapped or drifted under load" "$OUT/$name.continuity"
    fi
fi

# Every chunk must be stamped within a second of the host clock: a value from
# another clock (ns, a stream-relative time) would make the writer fill an
# enormous silence gap. The test prints the worst |pts - arrival| it saw.
if wanted clock; then
    name=clock
    if cargo test --quiet live_chunk_timestamps_are_on_the_host_clock -- --ignored --nocapture \
           >"$OUT/$name.check" 2>&1 \
       && grep -q '^clock:' "$OUT/$name.check"; then
        ok "$name" "$(grep '^clock:' "$OUT/$name.check" | sed 's/^clock: //')"
    else
        bad "$name" "a chunk timestamp was not on the host clock" "$OUT/$name.check"
    fi
fi

# Capture latency: the test spawns pw-play itself and reports the offset from
# the spawn to the first captured chunk's pts (pw-play start-up and the graph's
# buffering included).
if wanted latency; then
    name=latency
    mkwav "$OUT/tone.wav" --rate 48000 --secs 4 --level -6 --beep 0 >/dev/null
    if REC_AUDIO_TEST_WAV="$OUT/tone.wav" REC_AUDIO_TEST_TARGET=rec_test_sink \
           cargo test --quiet system_audio_capture_latency -- --ignored --nocapture \
           >"$OUT/$name.check" 2>&1 \
       && grep -q 'latency: first captured tone' "$OUT/$name.check"; then
        ok "$name" "$(grep 'latency: first captured tone' "$OUT/$name.check" | sed 's/^latency: //')"
    else
        bad "$name" "the latency test failed" "$OUT/$name.check"
    fi
fi

# The device disappearing mid-recording: the stream errors, the session keeps
# recording video and finalizes the file, and the warning says what happened.
if wanted unplug; then
    name=mic-unplug
    mkwav "$OUT/beeps-unplug.wav" --rate 48000 --secs 12 --level -6 --beep 0.04 >/dev/null
    ( sleep 0.5; exec pw-play --target rec_test_mic_play "$OUT/beeps-unplug.wav" ) \
        >"$OUT/$name.play.log" 2>&1 &
    player=$!
    ( sleep 2.0; kill "$HARNESS_LOOPBACK_PID" 2>/dev/null || true ) &
    killer=$!
    status=0
    timeout 90 "$REC" record -q --test-pattern --no-system-audio -t 5 \
        -o "$CLIPS/$name.mov" >"$OUT/$name.out" 2>"$OUT/$name.log" || status=$?
    wait "$player" 2>/dev/null || true
    wait "$killer" 2>/dev/null || true
    frames=$(ffprobe -v error -select_streams v -count_frames \
        -show_entries stream=nb_read_frames -of csv=p=0 "$CLIPS/$name.mov" 2>/dev/null || echo 0)
    if [ "$status" -eq 0 ] && [ "${frames:-0}" -gt 0 ] \
       && grep -q 'disconnected\|stream error' "$OUT/$name.log"; then
        ok "$name" "the recording finalized ($frames frames) after the mic vanished"
    else
        bad "$name" "status $status, $frames frames" "$OUT/$name.log"
    fi
fi

echo "$pass passed, $fail failed, $skipped skipped"
[ "$fail" -eq 0 ]
