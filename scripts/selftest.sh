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
        echo "h264-bitrate sync audio-gaps pause-resume gain compositor gui"
        echo "pattern-screen screen-region screen-x11 screen-window screen-token"
        echo "skipped: pattern-webcam pattern-mic scene-webcam"
        echo "         stream-rtmp stream-whip replay"
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

# The GPU compositor: every shape against the mask geometry, borders, the drop
# shadow, mirroring, placement, the camera scene, the Lanczos scaler against a
# CPU reference, against ffmpeg's Lanczos (PSNR) and on a 1-px pattern
# (aliasing), the BT.709 output, the pipelined readback and the golden images. It needs no display and no GPU: Mesa's software Vulkan device
# (lavapipe) runs it, and the tests skip with a printed reason only when there
# is no adapter at all.
if wanted compositor; then
    if command -v cargo >/dev/null 2>&1; then
        if CARGO_BUILD_JOBS=3 timeout "$TIMEOUT" cargo test --release gpu:: -- --nocapture \
               >"$OUT/compositor.log" 2>&1; then
            adapter=$(sed -n 's/^compositor tests run on: //p' "$OUT/compositor.log" | head -1)
            counts=$(sed -e 's/\x1b\[[0-9;]*m//g' "$OUT/compositor.log" \
                     | grep -oE '[0-9]+ passed; [0-9]+ failed' | tail -1)
            # The scaler against ffmpeg's Lanczos on the same RGB input: two
            # independent Lanczos3 implementations must agree to 40 dB.
            if CARGO_BUILD_JOBS=3 timeout "$TIMEOUT" cargo test --release gpu::tests::the_scaler_matches \
                   -- --ignored --nocapture >"$OUT/compositor-ffmpeg.log" 2>&1 \
               && grep -q 'PSNR' "$OUT/compositor-ffmpeg.log"; then
                psnr=$(grep -o 'PSNR [0-9.]*' "$OUT/compositor-ffmpeg.log" | tr '\n' ' ')
                echo "PASS  compositor  (${counts:-?} on ${adapter:-no adapter}; $psnr)"
                pass=$((pass+1))
            else
                echo "FAIL  compositor  (ffmpeg comparison; see $OUT/compositor-ffmpeg.log)"
                fail=$((fail+1))
            fi
        else
            echo "FAIL  compositor  (see $OUT/compositor.log)"; fail=$((fail+1))
        fi
    else
        echo "SKIP  compositor  (no cargo in this environment)"; skip=$((skip+1))
    fi
fi

# --- screen capture (phase 1A) ----------------------------------------------
# Both real pipelines, headless: a sway desktop with xdg-desktop-portal-wlr (the
# Wayland path: portal -> PipeWire) and Xvfb (the direct X11 path). Each harness
# paints something that moves, so a still or black capture fails.
#
# The cases need the dev image's sway/Xvfb/dbus; on a plain host they skip with
# the reason instead of failing.
if command -v dbus-run-session >/dev/null 2>&1 && command -v sway >/dev/null 2>&1 &&
   command -v Xvfb >/dev/null 2>&1 && [ -x scripts/screen/headless.sh ]; then
    screen_harness=yes
else
    screen_harness=""
fi
# `$REC` may be relative; the harness scripts run from another process.
REC_ABS=$(cd "$(dirname "$REC")" && pwd)/$(basename "$REC")

# Total and distinct video frames of a recording (ffmpeg's per-frame md5).
frames_total() { ffmpeg -v error -i "$1" -f framemd5 - 2>/dev/null | grep -vc '^#' || true; }
frames_distinct() { ffmpeg -v error -i "$1" -f framemd5 - 2>/dev/null | grep -v '^#' | cut -d, -f6 | sort -u | wc -l; }

# screen_case NAME FILE WIDTH HEIGHT MIN_DISTINCT: the recording must have the
# native size, a constant 60 fps and at least MIN_DISTINCT different frames.
screen_case() {
    name=$1; f=$2; w=$3; h=$4; move=$5
    got=$(ffprobe -v error -select_streams v -show_entries stream=width,height -of csv=p=0 "$f" 2>/dev/null | head -1)
    rate=$(ffprobe -v error -select_streams v -show_entries stream=r_frame_rate -of csv=p=0 "$f" 2>/dev/null | head -1)
    total=$(frames_total "$f")
    distinct=$(frames_distinct "$f")
    if [ "$got" = "$w,$h" ] && [ "$rate" = "60/1" ] && [ "$total" -ge 60 ] && [ "$distinct" -ge "$move" ]; then
        echo "PASS  $name  (${got} @ $rate, $total frames, $distinct distinct)"
        pass=$((pass+1))
    else
        echo "FAIL  $name  (size '$got' want $w,$h, rate '$rate', $total frames, $distinct distinct; see $OUT/$name.mp4.log)"
        fail=$((fail+1))
    fi
}

# screen_test NAME HARNESS CASE WIDTH HEIGHT MIN_DISTINCT
screen_test() {
    name=$1; harness=$2; case=$3; w=$4; h=$5; move=$6
    wanted "$name" || return 0
    if [ -z "$screen_harness" ]; then
        echo "SKIP  $name  (needs sway, Xvfb and dbus-run-session: run under scripts/dev.sh)"
        skip=$((skip+1))
        return 0
    fi
    if REC_BIN="$REC_ABS" OUT_DIR="$OUT" timeout "$TIMEOUT" "scripts/screen/$harness" \
           scripts/screen/cases.sh "$case" >/dev/null 2>"$OUT/$name.harness.log"; then
        screen_case "$name" "$OUT/$name.mp4" "$w" "$h" "$move"
    else
        echo "FAIL  $name  (see $OUT/$name.mp4.log and $OUT/$name.harness.log)"
        fail=$((fail+1))
    fi
}

# Wayland: portal + PipeWire, 1280x720 headless sway, restore token stored.
screen_test pattern-screen headless.sh portal 1280 720 100
# Wayland: --region crops the shared monitor (640x480 native pixels).
screen_test screen-region headless.sh region 640 480 40
# Wayland: `permissions --request` stores a grant; a corrupt token is recovered.
screen_test screen-token headless.sh token 1280 720 0
# X11: RandR enumeration and a root capture without a portal.
screen_test screen-x11 xvfb.sh x11 1280 800 30
# X11: --window <app> captures one client's own surface.
screen_test screen-window xvfb.sh window 400 300 30

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
wait \$app
echo "app exit=\$?" >"$d/exit"
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
           grep -q "app exit=0" "$d/exit" 2>/dev/null &&
           [ ! -e "$d/rt/rec-rs.pid" ]; then
            echo "PASS  gui  ($frames frames then $quit_frames on SIGTERM, $(grep -c '^track' "$d/probe.txt") tracks, preview shown ($(grep -m1 -o 'preview frame [0-9]*x[0-9]*' "$d/app.log")), second launch handed over, pause/resume/stop via the CLI, clean exit)"
            pass=$((pass+1))
        else
            echo "FAIL  gui  (file '$file' $frames frames, quit file '$quit_file' $quit_frames; see $d/app.log, $d/second.log, $d/controls.log, $d/session.log)"
            fail=$((fail+1))
        fi
    fi
fi

later() {
    wanted "$1" || return 0
    echo "SKIP  $1  ($2)"; skip=$((skip+1))
}
later pattern-webcam  "phase 1B: camera capture"
later scene-webcam    "phase 1G: the writer composites the camera over the screen"
later pattern-mic     "phase 1B: microphone capture"
later replay          "phase 1D: replay buffer"
later stream-rtmp     "phase 1D: RTMP"
later stream-whip     "phase 1D: WHIP"

echo "$pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
