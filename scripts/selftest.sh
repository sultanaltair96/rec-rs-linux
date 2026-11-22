#!/bin/sh
# Record short clips through every pipeline this phase has and verify them with
# `rec-rs probe` (GStreamer decodes every sample) and ffmpeg.
#
# Usage: scripts/selftest.sh [case...]      (default: every case)
#        scripts/selftest.sh --list
#
# Recordings go to ${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}, never into the
# repo. Cases that need a later phase print "SKIP <name> (phase 1X)".
#
# Adapted from rec-rs scripts/selftest.sh (MIT OR Apache-2.0): the macOS app,
# permission and streaming cases are replaced by the phase 0 cases and skips.
set -eu
cd "$(dirname "$0")/.."

if [ -n "${REC:-}" ]; then
    :
elif [ -x "${CARGO_TARGET_DIR:-target}/release/rec-rs" ]; then
    REC="${CARGO_TARGET_DIR:-target}/release/rec-rs"
else
    echo "selftest: no release binary found; run 'scripts/dev.sh cargo build --release' first" >&2
    exit 1
fi

OUT="${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}/selftest"
# Every recorder run is bounded: a hung capture device or encoder must fail the
# case, not the whole run (the dev container also has a ceiling, see dev.sh).
TIMEOUT="${REC_TIMEOUT:-120}"
rm -rf "$OUT"; mkdir -p "$OUT"
pass=0; fail=0; skip=0
only=" $* "
wanted() { [ -z "$(echo "$only" | tr -d ' ')" ] || [ "${only#* $1 }" != "$only" ]; }

case "${1:-}" in
    --list)
        echo "pattern-1080-h264 pattern-mp4 pattern-prores pattern-hevc pattern-4k-hevc"
        echo "h264-bitrate sync audio-gaps pause-resume gain gui"
        echo "skipped: pattern-screen pattern-webcam pattern-mic stream-rtmp stream-whip"
        echo "         replay"
        exit 0 ;;
esac

# record NAME [args...]: record, probe and fully decode with ffmpeg.
check() {
    name=$1; shift
    wanted "$name" || return 0
    case "$name" in *.mp4) f="$OUT/$name" ;; *) f="$OUT/$name.mov" ;; esac
    if timeout "$TIMEOUT" "$REC" record -q -o "$f" "$@" >/dev/null 2>"$OUT/$name.log" \
       && "$REC" probe "$f" >"$OUT/$name.probe" 2>&1 \
       && ffmpeg -v error -i "$f" -f null - 2>>"$OUT/$name.log"; then
        frames=$(ffprobe -v error -select_streams v -count_frames -show_entries stream=nb_read_frames -of csv=p=0 "$f")
        tracks=$(grep -c '^track' "$OUT/$name.probe")
        echo "PASS  $name  ($tracks tracks, $frames video frames)"; pass=$((pass+1))
    else
        echo "FAIL  $name  (see $OUT/$name.log / .probe)"; fail=$((fail+1))
    fi
}

# 1080p H.264 with system audio: the basic pipeline (video + AAC beeps).
check pattern-1080-h264 --test-pattern -t 3 --no-mic
# MP4 container instead of MOV.
check pattern-mp4 --test-pattern -t 2 --no-mic
# ProRes 422 (avenc_prores_ks; needs .mov).
check pattern-prores --test-pattern -t 2 --codec prores --no-mic
# HEVC Main (x265enc, RPM Fusion's gst-plugins-bad-freeworld).
check pattern-hevc --test-pattern -t 2 --codec hevc --no-mic
# 4K60 through the automatic codec choice: hevc above 1080p, upstream's rule.
check pattern-4k-hevc --test-pattern --noise -r 4k -t 2 --no-mic

# Bitrate and profile: a 1080p60 H.264 recording asked for 8 Mb/s must land
# within 25% of it, at profile High (upstream's H.264 profile; the
# free-repository fallback openh264enc can only do Constrained Baseline, and
# dev/ELEMENTS.md records that x265enc needs its VBV to stay on budget).
if wanted h264-bitrate; then
    f="$OUT/h264-bitrate.mp4"
    profile=""
    rate=0
    if timeout "$TIMEOUT" "$REC" record -q --test-pattern --noise --bitrate 8 -t 5 --no-mic -o "$f" \
           >/dev/null 2>"$OUT/h264-bitrate.log"; then
        profile=$(ffprobe -v error -select_streams v -show_entries stream=profile -of csv=p=0 "$f")
        rate=$(ffprobe -v error -select_streams v -show_entries stream=bit_rate -of csv=p=0 "$f")
    fi
    if [ "$profile" = High ] && awk -v r="$rate" 'BEGIN { exit !(r >= 6000000 && r <= 10000000) }'; then
        echo "PASS  h264-bitrate  (profile $profile, $(awk -v r="$rate" 'BEGIN { printf "%.2f", r / 1e6 }') Mb/s for --bitrate 8)"
        pass=$((pass+1))
    else
        echo "FAIL  h264-bitrate  (profile '$profile', ${rate} b/s; see $OUT/h264-bitrate.log)"
        fail=$((fail+1))
    fi
fi

# A/V sync: the test pattern flashes white and beeps at the same instant. The
# measured offset is reported either way.
#
# The first beep reads about +10 ms: GStreamer's muxer cannot write the AAC
# edit list that tells a decoder to skip the encoder's priming, so the writer
# drops those priming frames and re-stamps the rest (otherwise every beep is
# 42.7 ms late). The first kept frame then has no decoder history, so the very
# first beep fades in over its first ~10 ms. Every later beep reads 0.0 ms;
# see PARITY.md's audio track row.
if wanted sync; then
    if timeout "$TIMEOUT" "$REC" record -q --test-pattern --no-mic -t 5 -o "$OUT/sync.mov" >/dev/null 2>"$OUT/sync.log" \
       && python3 scripts/synccheck.py "$OUT/sync.mov" --max-ms 20 >"$OUT/sync.check" 2>&1; then
        echo "PASS  sync  ($(grep -m1 'audio 0' "$OUT/sync.check" | sed 's/.*offset //'))"; pass=$((pass+1))
    else
        echo "FAIL  sync  (see $OUT/sync.check)"; fail=$((fail+1))
    fi
fi

# A glitching device (every 50th audio buffer lost) must not shift audio: the
# writer fills the gap with silence.
if wanted audio-gaps; then
    if REC_SYNTH_DROP_AUDIO=50 timeout "$TIMEOUT" "$REC" record -q --test-pattern --no-mic -t 5 -o "$OUT/gaps.mov" >/dev/null 2>"$OUT/gaps.log" \
       && python3 scripts/synccheck.py "$OUT/gaps.mov" --max-ms 20 >"$OUT/gaps.check" 2>&1; then
        echo "PASS  audio-gaps  (dropped device buffers filled, still in sync)"; pass=$((pass+1))
    else
        echo "FAIL  audio-gaps  (see $OUT/gaps.check)"; fail=$((fail+1))
    fi
fi

# Pause/resume: 5 s of output with a pause in the middle. The file must have
# exactly 300 frames (5 s at 60 fps) and audio must still be in sync.
if wanted pause-resume; then
    timeout "$TIMEOUT" "$REC" record -q --test-pattern --no-mic -t 5 -o "$OUT/pause.mov" >/dev/null 2>"$OUT/pause.log" &
    recorder=$!
    sleep 2.3; "$REC" pause || true; sleep 2; "$REC" resume || true
    wait "$recorder" || true
    frames=$(ffprobe -v error -select_streams v -count_frames -show_entries stream=nb_read_frames -of csv=p=0 "$OUT/pause.mov" 2>/dev/null || echo 0)
    if python3 scripts/synccheck.py "$OUT/pause.mov" --max-ms 20 >"$OUT/pause.check" 2>&1 && [ "$frames" = 300 ]; then
        echo "PASS  pause-resume  ($frames frames, audio in sync across the pause)"; pass=$((pass+1))
    else
        echo "FAIL  pause-resume  ($frames frames; see $OUT/pause.check)"; fail=$((fail+1))
    fi
fi

# Levels: --system-gain reaches the system audio's own track (here the only
# one). The beep is generated at -6 dBFS; at -12 dB it must read -12 dB
# against that reference.
if wanted gain; then
    if timeout "$TIMEOUT" "$REC" record -q --test-pattern --no-mic --system-gain=-12 -t 3 -o "$OUT/gain.mov" >/dev/null 2>"$OUT/gain.log" \
       && python3 scripts/levelcheck.py "$OUT/gain.mov" --ref=-6 --expect=-12 >"$OUT/gain.check" 2>&1; then
        echo "PASS  gain  (--system-gain -12 on the system audio track)"; pass=$((pass+1))
    else
        echo "FAIL  gain  (see $OUT/gain.check)"; fail=$((fail+1))
    fi
fi

# The app: start the real GUI (window on headless sway, the software renderer)
# recording the test pattern, add a second launch (it must hand over to the
# running instance), stop it with `rec-rs stop` and check the finalized file.
# The tray and the global hotkeys need a desktop; this case proves the app
# degrades without them (the log says so) and that the recording is complete.
if wanted gui; then
    if ! command -v sway >/dev/null 2>&1; then
        echo "SKIP  gui  (needs sway; the dev image has it)"; skip=$((skip+1))
    else
        d="$OUT/gui"
        rm -rf "$d"; mkdir -p "$d/rt" "$d/out"; chmod 700 "$d/rt"
        printf '{"output_dir": "%s", "resolution": "1080p", "fps": 30, "mic_enabled": false, "system_audio": true}\n' \
            "$d/out" >"$d/settings.json"
        # One session script: sway and the app must die together, whatever the
        # case does.
        cat >"$d/run.sh" <<INNER
#!/bin/sh
export XDG_RUNTIME_DIR="$d/rt" REC_SETTINGS="$d/settings.json"
export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1
sway >"$d/sway.log" 2>&1 &
sway_pid=\$!
i=0
while [ ! -S "\$XDG_RUNTIME_DIR/wayland-1" ] && [ \$i -lt 150 ]; do sleep 0.1; i=\$((i+1)); done
[ -S "\$XDG_RUNTIME_DIR/wayland-1" ] || { kill \$sway_pid; exit 10; }
export WAYLAND_DISPLAY=wayland-1 REC_DEBUG=1
"$REC" gui --test-pattern --start-recording >"$d/app.log" 2>&1 &
app=\$!
sleep 7
# A second launch hands over to the running app instead of starting another.
"$REC" gui >"$d/second.log" 2>&1 || true
# The shell controls reach the app through the pidfile and signals.
"$REC" pause >"$d/controls.log" 2>&1 && echo paused >>"$d/controls.log" || echo "pause failed" >>"$d/controls.log"
sleep 1
"$REC" resume >>"$d/controls.log" 2>&1 && echo resumed >>"$d/controls.log" || echo "resume failed" >>"$d/controls.log"
sleep 1
"$REC" stop >"$d/stop.out" 2>>"$d/app.log" || true
# A second recording, ended by SIGTERM: quitting must finalize it first (the
# app prints the file's path on quit).
"$REC" gui --start-recording --test-pattern >>"$d/second.log" 2>&1 || true
sleep 5
kill -TERM \$app 2>/dev/null || true
wait \$app 2>/dev/null || true
kill -TERM \$sway_pid 2>/dev/null || true
sleep 0.5; kill -KILL \$sway_pid 2>/dev/null || true
exit 0
INNER
        chmod +x "$d/run.sh"
        if command -v dbus-run-session >/dev/null 2>&1; then
            timeout "$TIMEOUT" dbus-run-session -- "$d/run.sh" >"$d/session.log" 2>&1 || true
        else
            timeout "$TIMEOUT" "$d/run.sh" >"$d/session.log" 2>&1 || true
        fi
        file=$(cat "$d/stop.out" 2>/dev/null || true)
        quit_file=$(grep -o '/[^ ]*\.mov' "$d/app.log" 2>/dev/null | tail -1)
        frames=0; quit_frames=0
        [ -n "$file" ] && [ -f "$file" ] &&
            frames=$(ffprobe -v error -select_streams v -count_frames \
                -show_entries stream=nb_read_frames -of csv=p=0 "$file" 2>/dev/null || echo 0)
        [ -n "$quit_file" ] && [ -f "$quit_file" ] &&
            quit_frames=$(ffprobe -v error -select_streams v -count_frames \
                -show_entries stream=nb_read_frames -of csv=p=0 "$quit_file" 2>/dev/null || echo 0)
        if [ -n "$file" ] && [ -f "$file" ] && [ "$frames" -ge 90 ] &&
           [ -n "$quit_file" ] && [ -f "$quit_file" ] && [ "$quit_frames" -ge 30 ] &&
           "$REC" probe "$file" >"$d/probe.txt" 2>&1 &&
           "$REC" probe "$quit_file" >"$d/probe-quit.txt" 2>&1 &&
           grep -q "already running" "$d/second.log" 2>/dev/null &&
           grep -q "paused" "$d/controls.log" 2>/dev/null &&
           grep -q "resumed" "$d/controls.log" 2>/dev/null &&
           grep -q "app running" "$d/app.log" 2>/dev/null &&
           grep -q "preview frame" "$d/app.log" 2>/dev/null &&
           [ ! -e "$d/rt/rec-rs.pid" ]; then
            echo "PASS  gui  ($frames frames then $quit_frames on SIGTERM, $(grep -c '^track' "$d/probe.txt") tracks, preview shown ($(grep -m1 -o 'preview frame [0-9]*x[0-9]*' "$d/app.log")), second launch handed over, pause/resume/stop via the CLI)"
            pass=$((pass+1))
        else
            echo "FAIL  gui  (file '$file' $frames frames, quit file '$quit_file' $quit_frames; see $d/app.log, $d/second.log, $d/controls.log, $d/session.log)"
            fail=$((fail+1))
        fi
    fi
fi

# Cases that belong to later phases (they need capture or streaming).
later() {
    wanted "$1" || return 0
    echo "SKIP  $1  ($2)"; skip=$((skip+1))
}
later pattern-screen  "phase 1A: screen capture through the portal + PipeWire"
later pattern-webcam  "phase 1B: camera capture"
later pattern-mic     "phase 1B: microphone capture"
later replay          "phase 1D: replay buffer"
later stream-rtmp     "phase 1D: RTMP"
later stream-whip     "phase 1D: WHIP"

echo "$pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
