#!/bin/sh
# Record short clips through every pipeline this phase has and verify them with
# `rec-rs probe` (GStreamer decodes every sample), ffprobe and ffmpeg.
#
# Usage: scripts/selftest.sh [case...]      (default: every case)
#        scripts/selftest.sh --list
#
# Recordings go to ${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}, never into the
# repo. Cases that need a later phase print "SKIP <name> (phase 1X)".
#
# The writer cases check the file's track layout (video, mix, system stem, mic
# stem; alternate group and enabled flags; AAC priming edit lists), the codec
# and colour tags, the constant frame rate, A/V sync, the camera gate and lag,
# the scaler, the finalise and both crash states.
#
# `long-finalize` is a ten-minute recording that measures the finalise cost with
# hundreds of fragments; it is not run by default (name it explicitly).
#
# Adapted from rec-rs scripts/selftest.sh (MIT OR Apache-2.0): the macOS app,
# permission and streaming cases are replaced by this phase's cases and skips.
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
        echo "tracks audio-tracks sync audio-gaps pause-resume gain h264-bitrate"
        echo "scale camera camera-lag crash finalize-crash defrag timeout-stop"
        echo "opt-in: long-finalize (10 min)"
        echo "skipped: pattern-screen pattern-webcam device-mic stream-rtmp stream-whip"
        echo "         replay gui"
        exit 0 ;;
esac

# fail NAME WHY: record a failure with a one-line reason. It does not fail the
# shell (`set -e`), so the remaining cases still run.
bad() { echo "FAIL  $1  ($2)"; fail=$((fail+1)); }

# record NAME [args...]: record a short clip and set $f.
record() {
    name=$1; shift
    case "$name" in
        *.mp4) f="$OUT/$name" ;;
        pattern-mp4) f="$OUT/$name.mp4" ;;
        *) f="$OUT/$name.mov" ;;
    esac
    if timeout "$TIMEOUT" "$REC" record -q -o "$f" "$@" >/dev/null 2>"$OUT/$name.log"; then
        return 0
    fi
    bad "$name" "record failed, see $OUT/$name.log"
    return 1
}

# decodes FILE: probe and a full ffmpeg decode.
decodes() {
    "$REC" probe "$1" >"$1.probe" 2>&1 || { echo "probe failed ($1.probe)"; return 1; }
    ffmpeg -v error -i "$1" -f null - 2>>"$OUT/ffmpeg.log" || { echo "ffmpeg decode failed"; return 1; }
}

# frame_count FILE: decoded video frames.
frame_count() {
    ffprobe -v error -select_streams v -count_frames -show_entries stream=nb_read_frames -of csv=p=0 "$1"
}

# ff FIELD FILE [stream]: one ffprobe stream field, e.g. `ff profile $f`.
ff() {
    field=$1; file=$2; stream=${3:-v:0}
    ffprobe -v error -select_streams "$stream" -show_entries "stream=$field" -of csv=p=0 "$file" 2>/dev/null | head -1
}

# tracks FILE: number of tracks `rec-rs probe` reports.
tracks() { grep -c '^track' "$1.probe" 2>/dev/null || true; }

# track_line FILE N: the probe's line for track N.
track_line() { grep "^track $2 " "$1.probe" | head -1; }

# has TEXT NEEDLE: substring test.
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }

# audio_streams FILE: channels and kb/s of every audio stream, one per line
# (`2:256`). The measured bitrate of a 256 kb/s AAC track reads 255999.
audio_streams() {
    ffprobe -v error -select_streams a -show_entries stream=channels,bit_rate -of csv=p=0 "$1" \
        | awk -F, 'NF >= 2 { printf "%s:%.0f\n", $1, $2 / 1000 }'
}

# keyframe_gaps FILE: the video's keyframe interval in seconds, from ffprobe.
keyframe_gaps() {
    ffprobe -v error -select_streams v -skip_frame nokey -show_entries frame=pts_time -of csv=p=0 "$1" \
        | awk -F, 'NF { if (n) printf "%.3f\n", $1 - p; p = $1; n++ }'
}

# 1080p H.264 with system audio: the basic pipeline (video + AAC beeps).
if wanted pattern-1080-h264; then
    if record pattern-1080-h264 --test-pattern --no-mic -t 3 && decodes "$f"; then
        profile=$(ff profile "$f"); w=$(ff width "$f"); h=$(ff height "$f")
        prim=$(ff color_primaries "$f"); trc=$(ff color_transfer "$f"); spc=$(ff color_space "$f")
        rng=$(ff color_range "$f"); fps=$(ff r_frame_rate "$f")
        audio=$(audio_streams "$f")
        # A keyframe at least every 2 s (upstream). x264's scene-cut detection
        # may add one at a flash, so the gap is a maximum, not an exact 2 s.
        gap=$(keyframe_gaps "$f" | awk 'BEGIN { m = 0 } { if ($1 > m) m = $1 } END { printf "%.3f", m }')
        if [ "$(tracks "$f")" = 2 ] && [ "$profile" = High ] && [ "$w" = 1920 ] && [ "$h" = 1080 ] \
           && [ "$prim" = bt709 ] && [ "$trc" = bt709 ] && [ "$spc" = bt709 ] && [ "$rng" = tv ] \
           && [ "$fps" = 60/1 ] && has "$audio" "2:256" \
           && awk -v g="$gap" 'BEGIN { exit !(g > 0.1 && g < 2.2) }'; then
            echo "PASS  pattern-1080-h264  ($(frame_count "$f") frames, h264/High, bt709/tv, keyframe max ${gap}s, audio aac stereo 256 kb/s)"
            pass=$((pass+1))
        else
            bad pattern-1080-h264 "tracks=$(tracks "$f") profile=$profile ${w}x${h} $prim/$trc/$spc/$rng fps=$fps keyframe=${gap}s audio=$audio"
        fi
    fi
fi

# MP4 container instead of MOV.
if wanted pattern-mp4; then
    if record pattern-mp4 --test-pattern -t 2 --no-mic && decodes "$f" && [ "$(tracks "$f")" = 2 ]; then
        echo "PASS  pattern-mp4  (mp4 container, $(frame_count "$f") frames)"
        pass=$((pass+1))
    elif [ -f "$f" ]; then
        bad pattern-mp4 "tracks=$(tracks "$f")"
    fi
fi

# ProRes 422 (avenc_prores_ks, .mov only). The encoder is far below real time on
# a software-only host (dev/ELEMENTS.md), so the case checks the format, the
# profile and the decodability, not every grid slot.
if wanted pattern-prores; then
    if record pattern-prores --test-pattern -t 2 --codec prores --no-mic && decodes "$f"; then
        codec=$(ff codec_name "$f"); profile=$(ff profile "$f")
        prim=$(ff color_primaries "$f"); spc=$(ff color_space "$f")
        if [ "$codec" = prores ] && [ "$profile" = standard ] && [ "$prim" = bt709 ] && [ "$spc" = bt709 ] \
           && [ "$(tracks "$f")" = 2 ] && [ "$(frame_count "$f")" -ge 10 ]; then
            echo "PASS  pattern-prores  ($codec/$profile, bt709, $(frame_count "$f") frames)"
            pass=$((pass+1))
        else
            bad pattern-prores "codec=$codec profile=$profile $prim/$spc tracks=$(tracks "$f")"
        fi
    fi
fi

# HEVC Main (x265enc, RPM Fusion's gst-plugins-bad-freeworld).
if wanted pattern-hevc; then
    if record pattern-hevc --test-pattern -t 2 --codec hevc --no-mic && decodes "$f"; then
        codec=$(ff codec_name "$f"); profile=$(ff profile "$f")
        prim=$(ff color_primaries "$f"); trc=$(ff color_transfer "$f"); spc=$(ff color_space "$f")
        if [ "$codec" = hevc ] && [ "$profile" = Main ] && [ "$prim" = bt709 ] && [ "$trc" = bt709 ] \
           && [ "$spc" = bt709 ] && [ "$(tracks "$f")" = 2 ]; then
            echo "PASS  pattern-hevc  ($codec/$profile, bt709, $(frame_count "$f") frames)"
            pass=$((pass+1))
        else
            bad pattern-hevc "codec=$codec profile=$profile $prim/$trc/$spc"
        fi
    fi
fi

# 4K through the automatic codec choice: hevc above 1080p, upstream's rule.
# A software x265 cannot encode 4K60 on this host (11 fps measured), so the slot
# count is not asserted; the format, colour tags and tracks are.
if wanted pattern-4k-hevc; then
    if record pattern-4k-hevc --test-pattern --noise -r 4k -f 30 -t 2 --no-mic && decodes "$f"; then
        codec=$(ff codec_name "$f"); w=$(ff width "$f"); h=$(ff height "$f")
        prim=$(ff color_primaries "$f"); rng=$(ff color_range "$f")
        n=$(frame_count "$f")
        if [ "$codec" = hevc ] && [ "$w" = 3840 ] && [ "$h" = 2160 ] && [ "$prim" = bt709 ] \
           && [ "$rng" = tv ] && [ "$(tracks "$f")" = 2 ] && [ "$n" -ge 10 ]; then
            echo "PASS  pattern-4k-hevc  (-r 4k picked $codec, 3840x2160, bt709/tv, $n frames)"
            pass=$((pass+1))
        else
            bad pattern-4k-hevc "codec=$codec ${w}x${h} $prim/$rng tracks=$(tracks "$f") frames=$n"
        fi
    fi
fi

# Track layout with both sources: video, mix, system stem, mic stem. The
# synthetic mic beeps at 1.5 kHz, a different pitch from the pattern's 1 kHz.
# --system-gain=-12 makes the system stem distinguishable: the mix keeps the
# mic beep at 0 dB, so each track's level proves which source it carries.
if wanted tracks; then
    export REC_SYNTH_MIC=1
    if record tracks --test-pattern --mic default --system-gain=-12 -t 3 && decodes "$f"; then
        n=$(tracks "$f")
        l0=$(track_line "$f" 0); l1=$(track_line "$f" 1); l2=$(track_line "$f" 2); l3=$(track_line "$f" 3)
        mix_ok=1; level_ok=1
        if [ "$n" = 4 ] && has "$l1" "audio aac 48000 Hz, 2 ch  [mp4 #2 enabled alt-group 1" \
           && has "$l2" "audio aac 48000 Hz, 2 ch  [mp4 #3 disabled alt-group 1" \
           && has "$l3" "audio aac 48000 Hz, 1 ch  [mp4 #4 disabled alt-group 1" \
           && has "$l1" "elst media_time=2048" && has "$l2" "elst media_time=2048" \
           && has "$l3" "elst media_time=2048"; then
            mix_ok=0
        fi
        # mix: the mic beep at 0 dB; system stem: -12 dB; mic stem: 0 dB.
        python3 scripts/levelcheck.py "$f" --audio 0 --ref=-6 --expect=0 >"$OUT/tracks.mix" 2>&1 || level_ok=0
        python3 scripts/levelcheck.py "$f" --audio 1 --ref=-6 --expect=-12 >"$OUT/tracks.sys" 2>&1 || level_ok=0
        python3 scripts/levelcheck.py "$f" --audio 2 --ref=-6 --expect=0 >"$OUT/tracks.mic" 2>&1 || level_ok=0
        # The mic stem is mono 192 kb/s, the system stem stereo 256 kb/s.
        streams=$(audio_streams "$f")
        if [ "$mix_ok" = 0 ] && [ "$level_ok" = 1 ] && has "$streams" "2:256" && has "$streams" "1:192"; then
            echo "PASS  tracks  (4 tracks: video, mix, system, mic; alt-group 1, mix enabled; elst 2048 on every audio track; levels mix 0 dB, system -12 dB, mic 0 dB)"
            pass=$((pass+1))
        else
            bad tracks "layout_ok=$mix_ok levels_ok=$level_ok streams=$streams"
            grep -E "^(track|levels)" "$f.probe" "$OUT"/tracks.mix "$OUT"/tracks.sys "$OUT"/tracks.mic 2>/dev/null | sed 's/^/    /' | head -20
        fi
    fi
    unset REC_SYNTH_MIC
fi

# --audio-tracks mix (one AAC track) and stems (no mix, every track enabled).
if wanted audio-tracks; then
    ok=1
    if record audio-tracks-mix --test-pattern --mic default --audio-tracks mix -t 2 && decodes "$f"; then
        [ "$(tracks "$f")" = 2 ] || ok=0
        has "$(track_line "$f" 1)" "[mp4 #2 enabled alt-group 0" || ok=0
    else
        ok=0
    fi
    f=$OUT/audio-tracks-stems.mov
    export REC_SYNTH_MIC=1
    if timeout "$TIMEOUT" "$REC" record -q --test-pattern --mic default --audio-tracks stems -t 2 -o "$f" >/dev/null 2>"$OUT/audio-tracks-stems.log" && decodes "$f"; then
        [ "$(tracks "$f")" = 3 ] || ok=0
        # No group: both stems are enabled alternatives of a normal movie.
        has "$(track_line "$f" 1)" "[mp4 #2 enabled alt-group 0" || ok=0
        has "$(track_line "$f" 2)" "[mp4 #3 enabled alt-group 0" || ok=0
    else
        ok=0
    fi
    unset REC_SYNTH_MIC
    if [ "$ok" = 1 ]; then
        echo "PASS  audio-tracks  (mix: 2 tracks; stems: video + 2 enabled stems, no alternate group)"
        pass=$((pass+1))
    else
        bad audio-tracks "see audio-tracks-*.probe"
    fi
fi

# A/V sync: the test pattern flashes white and beeps at the same instant. With
# the AAC priming skipped by the edit list, the first beep is exact too, so the
# bar is upstream's 1 ms.
if wanted sync; then
    if record sync --test-pattern --no-mic -t 5 && python3 scripts/synccheck.py "$f" --max-ms 1 >"$OUT/sync.check" 2>&1; then
        echo "PASS  sync  ($(grep -m1 'audio 0' "$OUT/sync.check" | sed 's/.*offset //'))"
        pass=$((pass+1))
    elif [ -f "$f" ]; then
        bad sync "see $OUT/sync.check"
    fi
fi

# A glitching device (every 50th audio buffer lost) must not shift audio: the
# writer fills the gap with silence.
if wanted audio-gaps; then
    if REC_SYNTH_DROP_AUDIO=50 record audio-gaps --test-pattern --no-mic -t 5 \
       && python3 scripts/synccheck.py "$f" --max-ms 1 >"$OUT/audio-gaps.check" 2>&1; then
        echo "PASS  audio-gaps  (dropped device buffers filled, still in sync)"
        pass=$((pass+1))
    elif [ -f "$f" ]; then
        bad audio-gaps "see $OUT/audio-gaps.check"
    fi
fi

# Pause/resume: 5 s of output with a pause in the middle. The file must have
# exactly 300 frames (5 s at 60 fps) and audio must still be in sync.
if wanted pause-resume; then
    f="$OUT/pause-resume.mov"
    timeout "$TIMEOUT" "$REC" record -q --test-pattern --no-mic -t 5 -o "$f" >/dev/null 2>"$OUT/pause-resume.log" &
    recorder=$!
    sleep 2.3; "$REC" pause >/dev/null 2>&1 || true; sleep 2; "$REC" resume >/dev/null 2>&1 || true
    wait "$recorder" || true
    frames=$(ffprobe -v error -select_streams v -count_frames -show_entries stream=nb_read_frames -of csv=p=0 "$f" 2>/dev/null || echo 0)
    if python3 scripts/synccheck.py "$f" --max-ms 1 >"$OUT/pause-resume.check" 2>&1 && [ "$frames" = 300 ]; then
        echo "PASS  pause-resume  ($frames frames, audio in sync across the pause)"
        pass=$((pass+1))
    else
        bad pause-resume "$frames frames; see $OUT/pause-resume.check"
    fi
fi

# Levels: the faders reach each source's own stem. The pattern beeps at
# -6 dBFS, the synthetic mic at -6 dBFS too; with different faders the two
# stems must read exactly those levels.
if wanted gain; then
    export REC_SYNTH_MIC=1
    if record gain --test-pattern --mic default --system-gain=-12 --mic-gain=-18 -t 3 \
       && python3 scripts/levelcheck.py "$f" --audio 1 --ref=-6 --expect=-12 >"$OUT/gain.sys" 2>&1 \
       && python3 scripts/levelcheck.py "$f" --audio 2 --ref=-6 --expect=-18 >"$OUT/gain.mic" 2>&1; then
        echo "PASS  gain  (system stem -12 dB, mic stem -18 dB)"
        pass=$((pass+1))
    elif [ -f "$f" ]; then
        bad gain "see $OUT/gain.sys / $OUT/gain.mic"
    fi
    unset REC_SYNTH_MIC
fi

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
        profile=$(ff profile "$f")
        rate=$(ff bit_rate "$f")
    fi
    if [ "$profile" = High ] && awk -v r="$rate" 'BEGIN { exit !(r >= 6000000 && r <= 10000000) }'; then
        echo "PASS  h264-bitrate  (profile $profile, $(awk -v r="$rate" 'BEGIN { printf "%.2f", r / 1e6 }') Mb/s for --bitrate 8)"
        pass=$((pass+1))
    else
        bad h264-bitrate "profile '$profile', ${rate} b/s; see $OUT/h264-bitrate.log"
    fi
fi

# The scaler: a source larger than the output (a 1440p display into a 1080p
# recording) is scaled with Lanczos, and every frame is on the grid.
if wanted scale; then
    export REC_SYNTH_NATIVE=2560x1440
    if record scale --test-pattern --no-mic -t 2 && decodes "$f"; then
        w=$(ff width "$f"); h=$(ff height "$f"); n=$(frame_count "$f")
        if [ "$w" = 1920 ] && [ "$h" = 1080 ] && [ "$n" = 120 ]; then
            echo "PASS  scale  (2560x1440 source -> ${w}x${h} output, $n frames)"
            pass=$((pass+1))
        else
            bad scale "${w}x${h}, $n frames (expected 1920x1080, 120)"
        fi
    fi
    unset REC_SYNTH_NATIVE
fi

# The camera: the synthetic camera holds the start for its 2 s settle, then its
# red frames are what the file contains (--scene cam). Wall time proves the
# gate ran; the first frame's colour proves the scene wiring.
if wanted camera; then
    f="$OUT/camera.mov"
    start=$(date +%s%N)
    export REC_SYNTH_CAM=1
    if timeout "$TIMEOUT" "$REC" record -q --test-pattern --webcam --scene cam --no-mic -t 2 -o "$f" >/dev/null 2>"$OUT/camera.log"; then
        end=$(date +%s%N); ms=$(( (end - start) / 1000000 ))
        n=$(frame_count "$f")
        rgb=$(ffmpeg -v error -i "$f" -frames:v 1 -f rawvideo -pix_fmt rgb24 - 2>/dev/null | python3 -c '
import sys
b = sys.stdin.buffer.read(); n = max(1, len(b) // 3)
print("%.0f %.0f %.0f" % (sum(b[0::3])/n, sum(b[1::3])/n, sum(b[2::3])/n))')
        r=$(echo "$rgb" | cut -d' ' -f1)
        # At least 2 s of settle + 2 s of file, and a red first frame.
        if [ "$ms" -ge 3500 ] && [ "$n" = 120 ] && awk -v r="$r" 'BEGIN { exit !(r > 150) }'; then
            echo "PASS  camera  (2 s settle then $n frames of camera content in ${ms} ms, first frame RGB $rgb)"
            pass=$((pass+1))
        else
            bad camera "${ms} ms, $n frames, first frame RGB $rgb"
        fi
    fi
    unset REC_SYNTH_CAM
fi

# A camera delivering 80 ms late: emission waits for it (the lag adapts) and
# the file still has every frame on the grid.
if wanted camera-lag; then
    export REC_SYNTH_CAM=1 REC_SYNTH_CAM_LATENCY_MS=80
    if record camera-lag --test-pattern --webcam --scene cam --no-mic -t 2 && decodes "$f"; then
        n=$(frame_count "$f")
        if [ "$n" = 120 ]; then
            echo "PASS  camera-lag  (80 ms late camera, $n frames, no drift)"
            pass=$((pass+1))
        else
            bad camera-lag "$n frames (expected 120)"
        fi
    fi
    unset REC_SYNTH_CAM REC_SYNTH_CAM_LATENCY_MS
fi

# Crash safety: SIGKILL mid-recording leaves a playable fragmented file.
if wanted crash; then
    f="$OUT/crash.mov"
    timeout "$TIMEOUT" "$REC" record -q --test-pattern --no-mic -t 10 -o "$f" >/dev/null 2>"$OUT/crash.log" &
    pid=$!
    sleep 4.5
    kill -9 "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    if [ -s "$f" ] && ffmpeg -v error -i "$f" -f null - 2>>"$OUT/crash.log" \
       && "$REC" probe "$f" >"$OUT/crash.probe" 2>&1 && grep -q '^fragmented: yes' "$OUT/crash.probe"; then
        frames=$(frame_count "$f" 2>/dev/null || echo 0)
        echo "PASS  crash  (SIGKILL at 4.5 s, $frames frames, still fragmented and playable)"
        pass=$((pass+1))
    else
        bad crash "see $OUT/crash.log"
    fi
fi

# The finalise's own crash windows: after the regular moov is appended (two
# moovs, the first still plays) and in the middle of flipping the fragments.
if wanted finalize-crash; then
    ok=1
    for mode in before-flip mid-flip; do
        f="$OUT/finalize-$mode.mov"
        REC_DEFRAG_CRASH=$mode timeout "$TIMEOUT" "$REC" record -q --test-pattern --no-mic -t 2 -o "$f" >/dev/null 2>"$OUT/finalize-$mode.log" || true
        # The process is killed on purpose (137); the file must still decode.
        if [ ! -s "$f" ] || ! ffmpeg -v error -i "$f" -f null - 2>>"$OUT/finalize-$mode.log"; then
            ok=0
        fi
        [ "$mode" = before-flip ] && [ "$(frame_count "$f")" != 120 ] && ok=0
    done
    if [ "$ok" = 1 ]; then
        echo "PASS  finalize-crash  (before the flip: all 120 frames; mid-flip: playable)"
        pass=$((pass+1))
    else
        bad finalize-crash "see $OUT/finalize-*.log"
    fi
fi

# The finalise changes no media bytes: the kept fragmented copy and the final
# regular movie decode to the same frames, and the final file has no fragments
# left (no `moof`, no `mvex`).
if wanted defrag; then
    f="$OUT/defrag.mov"; frag="$OUT/defrag-fragments.mov"
    rm -f "$f" "$frag"
    if REC_DEFRAG_KEEP="$frag" REC_AAC_PRIMING=0 record defrag --test-pattern --no-mic -t 2; then
        n1=$(frame_count "$f"); n2=$(frame_count "$frag")
        h1=$(ffmpeg -v error -i "$f" -map 0:v:0 -f framemd5 - 2>/dev/null | md5sum | cut -d' ' -f1)
        h2=$(ffmpeg -v error -i "$frag" -map 0:v:0 -f framemd5 - 2>/dev/null | md5sum | cut -d' ' -f1)
        kinds=$(python3 - "$f" "$frag" <<'PY2'
import struct, sys
def kinds(path):
    b = open(path, "rb").read(); off = 0; out = []
    while off + 8 <= len(b):
        s = struct.unpack(">I", b[off:off+4])[0]; out.append(b[off+4:off+8].decode("latin1")); off += s
    return out
final, frag = kinds(sys.argv[1]), kinds(sys.argv[2])
print("final:" + ",".join(final))
print("frag:" + ",".join(frag))
PY2
)
        final_kinds=$(echo "$kinds" | sed -n 's/^final://p')
        frag_kinds=$(echo "$kinds" | sed -n 's/^frag://p')
        layout_ok=0
        if has "$final_kinds" "moov" && has "$frag_kinds" "moof" && has "$frag_kinds" "moov"; then
            if ! has "$final_kinds" "moof" && ! has "$final_kinds" "mvex"; then
                layout_ok=1
            fi
        fi
        if [ "$layout_ok" = 1 ] && [ "$n1" = "$n2" ] && [ "$n1" = 120 ] && [ "$h1" = "$h2" ]; then
            echo "PASS  defrag  ($n1 samples both ways, identical frame hashes; regular movie, no moof/mvex)"
            pass=$((pass+1))
        else
            bad defrag "frames $n1/$n2, hashes $h1/$h2, final=$final_kinds frag=$frag_kinds"
        fi
    fi
fi

# The writer through SIGINT (`rec-rs stop`): the file is finalised, not lost.
if wanted timeout-stop; then
    f="$OUT/timeout-stop.mov"
    timeout "$TIMEOUT" "$REC" record -q --test-pattern --no-mic -o "$f" >/dev/null 2>"$OUT/timeout-stop.log" &
    pid=$!
    sleep 2
    if "$REC" stop >"$OUT/timeout-stop.out" 2>>"$OUT/timeout-stop.log"; then
        sleep 0.3
        frames=$(frame_count "$f" 2>/dev/null || echo 0)
        if [ "$frames" -ge 60 ] && ffmpeg -v error -i "$f" -f null - 2>>"$OUT/timeout-stop.log"; then
            echo "PASS  timeout-stop  ($frames frames after \`rec-rs stop\`)"
            pass=$((pass+1))
        else
            bad timeout-stop "$frames frames"
        fi
    else
        bad timeout-stop "rec-rs stop failed"
    fi
    wait "$pid" 2>/dev/null || true
fi

# Ten minutes at a small size: measures the finalise with hundreds of
# fragments. Opt-in: name it explicitly (`scripts/selftest.sh long-finalize`).
if wanted long-finalize; then
    f="$OUT/long-finalize.mp4"
    start=$(date +%s%N)
    export REC_DEBUG=1
    if timeout 900 "$REC" record -q --test-pattern --no-mic -r 480x270 -t 600 -o "$f" >/dev/null 2>"$OUT/long-finalize.log"; then
        end=$(date +%s%N); wall=$(( (end - start) / 1000000 ))
        finalise=$(sed -n 's/.*finalise .* in \([0-9]*\) ms/\1/p' "$OUT/long-finalize.log" | tail -1)
        frags=$(sed -n 's/.*finalise [a-z]* (\([0-9]*\) fragments.*/\1/p' "$OUT/long-finalize.log" | tail -1)
        n=$(frame_count "$f")
        if [ -n "$finalise" ] && [ "$n" -ge 35000 ]; then
            echo "PASS  long-finalize  (10 min, $frags fragments, $n frames; finalise ${finalise} ms; record+finalise $((wall/1000)) s)"
            pass=$((pass+1))
        else
            bad long-finalize "finalise=${finalise}ms fragments=$frags frames=$n"
        fi
    else
        bad long-finalize "see $OUT/long-finalize.log"
    fi
    unset REC_DEBUG
fi

# Cases that belong to later phases (they need a real device, the app or
# streaming; the synthetic equivalents above cover the writer's side).
later() {
    wanted "$1" || return 0
    echo "SKIP  $1  ($2)"; skip=$((skip+1))
}
later pattern-screen  "phase 1A: real screen capture through the portal + PipeWire"
later pattern-webcam  "needs a real desktop: a V4L2 camera (the synthetic one is covered by 'camera')"
later device-mic      "needs a real desktop: a PipeWire microphone (the synthetic one is covered by 'tracks')"
later replay          "phase 1D: replay buffer"
later stream-rtmp     "phase 1D: RTMP"
later stream-whip     "phase 1D: WHIP"
later gui             "phase 1E: the app"

echo "$pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
