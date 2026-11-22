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
rm -rf "$OUT"; mkdir -p "$OUT"
pass=0; fail=0; skip=0
only=" $* "
wanted() { [ -z "$(echo "$only" | tr -d ' ')" ] || [ "${only#* $1 }" != "$only" ]; }

case "${1:-}" in
    --list)
        echo "pattern-1080-h264 pattern-mp4 pattern-prores sync audio-gaps pause-resume gain"
        echo "skipped: pattern-4k-hevc pattern-hevc pattern-screen pattern-webcam pattern-mic"
        echo "         stream-rtmp stream-whip replay gui"
        exit 0 ;;
esac

# record NAME [args...]: record, probe and fully decode with ffmpeg.
check() {
    name=$1; shift
    wanted "$name" || return 0
    case "$name" in *.mp4) f="$OUT/$name" ;; *) f="$OUT/$name.mov" ;; esac
    if "$REC" record -q -o "$f" "$@" >/dev/null 2>"$OUT/$name.log" \
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

# H.264 costs few bits and the encoder is openh264enc in Fedora's free repos;
# there is no HEVC encoder there at all (dev/ELEMENTS.md).
if wanted pattern-hevc; then
    echo "SKIP  pattern-hevc  (phase 1C: no HEVC encoder in Fedora's free repos; dev/ELEMENTS.md)"; skip=$((skip+1))
fi
if wanted pattern-4k-hevc; then
    echo "SKIP  pattern-4k-hevc  (phase 1C: no HEVC encoder in Fedora's free repos; dev/ELEMENTS.md)"; skip=$((skip+1))
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
    if "$REC" record -q --test-pattern --no-mic -t 5 -o "$OUT/sync.mov" >/dev/null 2>"$OUT/sync.log" \
       && python3 scripts/synccheck.py "$OUT/sync.mov" --max-ms 20 >"$OUT/sync.check" 2>&1; then
        echo "PASS  sync  ($(grep -m1 'audio 0' "$OUT/sync.check" | sed 's/.*offset //'))"; pass=$((pass+1))
    else
        echo "FAIL  sync  (see $OUT/sync.check)"; fail=$((fail+1))
    fi
fi

# A glitching device (every 50th audio buffer lost) must not shift audio: the
# writer fills the gap with silence.
if wanted audio-gaps; then
    if REC_SYNTH_DROP_AUDIO=50 "$REC" record -q --test-pattern --no-mic -t 5 -o "$OUT/gaps.mov" >/dev/null 2>"$OUT/gaps.log" \
       && python3 scripts/synccheck.py "$OUT/gaps.mov" --max-ms 20 >"$OUT/gaps.check" 2>&1; then
        echo "PASS  audio-gaps  (dropped device buffers filled, still in sync)"; pass=$((pass+1))
    else
        echo "FAIL  audio-gaps  (see $OUT/gaps.check)"; fail=$((fail+1))
    fi
fi

# Pause/resume: 5 s of output with a pause in the middle. The file must have
# exactly 300 frames (5 s at 60 fps) and audio must still be in sync.
if wanted pause-resume; then
    "$REC" record -q --test-pattern --no-mic -t 5 -o "$OUT/pause.mov" >/dev/null 2>"$OUT/pause.log" &
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
    if "$REC" record -q --test-pattern --no-mic --system-gain=-12 -t 3 -o "$OUT/gain.mov" >/dev/null 2>"$OUT/gain.log" \
       && python3 scripts/levelcheck.py "$OUT/gain.mov" --ref=-6 --expect=-12 >"$OUT/gain.check" 2>&1; then
        echo "PASS  gain  (--system-gain -12 on the system audio track)"; pass=$((pass+1))
    else
        echo "FAIL  gain  (see $OUT/gain.check)"; fail=$((fail+1))
    fi
fi

# Cases that belong to later phases (they need capture, the app or streaming).
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
later gui             "phase 1E: the app"

echo "$pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
