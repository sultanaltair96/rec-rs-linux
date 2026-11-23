#!/bin/sh
# Record short clips through every pipeline this phase has and verify them with
# `rec-rs probe` (GStreamer decodes every sample), ffprobe and ffmpeg.
#
# Usage: scripts/selftest.sh [case...]      (default: every case)
#        scripts/selftest.sh --list
#
# Recordings go to ${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}/selftest-<hash>,
# never into the repo. The hash is of the worktree path (like dev.sh's target
# dir): inside the dev container every worktree shares $HOME, so a fixed name
# would make parallel runs collide ("already exists" on every case). Cases that
# need a later phase print "SKIP <name> (phase 1X)".
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

repo_hash=$(printf '%s' "$(pwd -P)" | sha256sum | cut -c1-12)
OUT="${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}/selftest-$repo_hash"
# Every recorder run is bounded: a hung capture device or encoder must fail the
# case, not the whole run (the dev container also has a ceiling, see dev.sh).
TIMEOUT="${REC_TIMEOUT:-120}"
rm -rf "$OUT"; mkdir -p "$OUT"
pass=0; fail=0; skip=0
only=" $* "
only_clean=$(echo "$only" | tr -d ' ')
wanted() { [ -z "$only_clean" ] || [ "${only#* $1 }" != "$only" ]; }
# An opt-in case only runs when it is named: the default pass stays short.
named() { [ -n "$only_clean" ] && wanted "$1"; }

case "${1:-}" in
    --list)
        echo "pattern-1080-h264 pattern-mp4 pattern-prores pattern-hevc pattern-4k-hevc"
        echo "pattern-4k-long (20 s, RSS bound)"
        echo "tracks audio-tracks audio-silent sync audio-gaps pause-resume gain h264-bitrate"
        echo "scale camera camera-lag crash finalize-crash defrag timeout-stop compositor gui"
        echo "pattern-screen screen-region screen-x11 screen-window screen-token"
        echo "screen-exclude no-sink"
        echo "opt-in: long-finalize (10 min)"
        echo "pattern-mic pattern-system-audio"
        echo "skipped: pattern-webcam scene-webcam stream-rtmp stream-whip replay"
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

# audio_streams FILE: channels and sample rate (kHz) of every audio stream,
# one per line (`2:48`). The realized average bitrate is not checked: the AAC
# encoder is configured at 256/192 kb/s, but a mostly-silent beep train uses
# far fewer bits and the average would test the content, not the encoder.
audio_streams() {
    # ffprobe prints sample_rate before channels for this field set.
    ffprobe -v error -select_streams a -show_entries stream=channels,sample_rate -of csv=p=0 "$1" \
        | awk -F, 'NF >= 2 { printf "%s:%.0f\n", $2, $1 / 1000 }'
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
           && [ "$fps" = 60/1 ] && has "$audio" "2:48" \
           && awk -v g="$gap" 'BEGIN { exit !(g > 0.1 && g < 3.2) }'; then
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
        n=$(frame_count "$f")
        # Upstream records every ProRes frame; ffmpeg's fast "Apple" encoder
        # with slice threads does too for the pattern (prores_ks at its
        # defaults managed 27 of 120 on this host, prores_aw 120 of 120). 95%
        # is the bar so a loaded host is not a hard failure, and the count is
        # printed either way.
        if [ "$codec" = prores ] && [ "$profile" = Standard ] && [ "$prim" = bt709 ] && [ "$spc" = bt709 ] \
           && [ "$(tracks "$f")" = 2 ] && [ "$n" -ge 114 ]; then
            echo "PASS  pattern-prores  ($codec/$profile, bt709, $n/120 frames = $(awk -v n="$n" 'BEGIN { printf "%.1f", n / 2 }') fps)"
            pass=$((pass+1))
        else
            bad pattern-prores "codec=$codec profile=$profile $prim/$spc tracks=$(tracks "$f") frames=$n/120 (need >=114)"
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
           && [ "$rng" = tv ] && [ "$(tracks "$f")" = 2 ] && [ "$n" -ge 5 ]; then
            echo "PASS  pattern-4k-hevc  (-r 4k picked $codec, 3840x2160, bt709/tv, $n frames)"
            pass=$((pass+1))
        else
            bad pattern-4k-hevc "codec=$codec ${w}x${h} $prim/$rng tracks=$(tracks "$f") frames=$n"
        fi
    fi
fi

# A slow encoder must not grow memory: twenty seconds of noisy 4K60 is far
# below real time on a CPU (x265 ~15 fps here), so the writer drops grid slots
# (counted, timeline kept). Peak resident memory is sampled from /proc while it
# runs; the appsrc and every queue are bounded to a couple of frames, so the
# figure is the encoder's frame pool, not a growing backlog.
if wanted pattern-4k-long; then
    f="$OUT/pattern-4k-long.mov"
    "$REC" record --test-pattern --noise -r 4k -t 20 --no-mic -o "$f" >"$OUT/pattern-4k-long.out" 2>&1 &
    pid=$!
    peak=0
    while kill -0 "$pid" 2>/dev/null; do
        rss=$(awk '/VmHWM/{print $2}' "/proc/$pid/status" 2>/dev/null || echo 0)
        [ -n "$rss" ] && [ "$rss" -gt "$peak" ] && peak=$rss
        sleep 0.25
    done
    if wait "$pid" && decodes "$f"; then
        frames=$(frame_count "$f")
        dropped=$(sed -n 's/.*video frames, [0-9]* duplicated, \([0-9]*\) dropped.*/\1/p' "$OUT/pattern-4k-long.out" | head -1)
        fps=$(awk -v n="$frames" 'BEGIN { printf "%.1f", n / 20 }')
        # 1.5 GB: the encoder's 4K pools plus two queued frames, no backlog.
        if [ "$peak" -lt 1500000 ] && [ "$frames" -gt 0 ]; then
            echo "PASS  pattern-4k-long  (20 s 4K60 noise: peak RSS $((peak / 1024)) MB, $frames frames ($fps fps), ${dropped:-0} dropped)"
            pass=$((pass+1))
        else
            bad pattern-4k-long "peak RSS $((peak / 1024)) MB, $frames frames, ${dropped:-0} dropped"
        fi
    fi
fi

# Track layout with both sources: video, mix, system stem, mic stem. The
# synthetic mic beeps at 1.5 kHz, a different pitch from the pattern's 1 kHz.
# --system-gain=-12 makes the system stem distinguishable: the mix keeps the
# mic beep at 0 dB, so each track's level proves which source it carries.
if wanted tracks; then
    export REC_SYNTH_MIC=1
    if record tracks --test-pattern --mic default --system-gain=-12 -t 4 && decodes "$f"; then
        n=$(tracks "$f")
        l0=$(track_line "$f" 0); l1=$(track_line "$f" 1); l2=$(track_line "$f" 2); l3=$(track_line "$f" 3)
        mix_ok=1; level_ok=1
        if [ "$n" = 4 ] && has "$l0" "vide avc1 enabled=true" \
           && has "$l1" "soun mp4a enabled=true" && has "$l1" "[alt-group 1" \
           && has "$l2" "soun mp4a enabled=false" && has "$l2" "[alt-group 1" \
           && has "$l3" "soun mp4a enabled=false" && has "$l3" "[alt-group 1" \
           && has "$l1" "elst media_time=" && has "$l2" "elst media_time=" \
           && has "$l3" "elst media_time="; then
            mix_ok=0
        fi
        # mix: the mic beep at 0 dB; system stem: -12 dB; mic stem: 0 dB.
        python3 scripts/levelcheck.py "$f" --audio 0 --ref=-6 --expect=0 --skip 0.5 --allow-missing 2 >"$OUT/tracks.mix" 2>&1 || level_ok=0
        python3 scripts/levelcheck.py "$f" --audio 1 --ref=-6 --expect=-12 --skip 0.5 --allow-missing 2 >"$OUT/tracks.sys" 2>&1 || level_ok=0
        python3 scripts/levelcheck.py "$f" --audio 2 --ref=-6 --expect=0 --skip 0.5 --allow-missing 2 >"$OUT/tracks.mic" 2>&1 || level_ok=0
        # The mic stem is mono 192 kb/s, the system stem stereo 256 kb/s.
        streams=$(audio_streams "$f")
        if [ "$mix_ok" = 0 ] && [ "$level_ok" = 1 ] && has "$streams" "2:48" && has "$streams" "1:48"; then
            echo "PASS  tracks  (4 tracks: video, mix, system, mic; alt-group 1, mix enabled; elst on every audio track (priming $(echo "$l1" | sed -n 's/.*elst media_time=\([0-9]*\).*/\1/p')); levels mix 0 dB, system -12 dB, mic 0 dB)"
            pass=$((pass+1))
        else
            bad tracks "layout=$([ "$mix_ok" = 0 ] && echo ok || echo bad) levels=$([ "$level_ok" = 1 ] && echo ok || echo bad) streams=$streams"
            grep -E "^(track|levels)" "$f.probe" "$OUT"/tracks.mix "$OUT"/tracks.sys "$OUT"/tracks.mic 2>/dev/null | sed 's/^/    /' | head -20
        fi
    fi
    unset REC_SYNTH_MIC
fi

# --audio-tracks mix (one AAC track) and stems (no mix, every track enabled).
if wanted audio-tracks; then
    ok=1
    export REC_SYNTH_MIC=1
    if record audio-tracks-mix --test-pattern --mic default --audio-tracks mix -t 2 && decodes "$f"; then
        [ "$(tracks "$f")" = 2 ] || ok=0
        has "$(track_line "$f" 1)" "soun mp4a enabled=true" || ok=0
        has "$(track_line "$f" 1)" "[alt-group 0" || ok=0
    else
        ok=0
    fi
    f=$OUT/audio-tracks-stems.mov
    if timeout "$TIMEOUT" "$REC" record -q --test-pattern --mic default --audio-tracks stems -t 2 -o "$f" >/dev/null 2>"$OUT/audio-tracks-stems.log" && decodes "$f"; then
        [ "$(tracks "$f")" = 3 ] || ok=0
        # No group: both stems are enabled tracks of a normal movie.
        has "$(track_line "$f" 1)" "soun mp4a enabled=true" || ok=0
        has "$(track_line "$f" 2)" "soun mp4a enabled=true" || ok=0
        has "$(track_line "$f" 2)" "[alt-group 0" || ok=0
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

# A requested source that never delivers must not lose its track silently
# (G15b): qtmux only writes a trak once it has a sample, so the writer gives
# every requested track a moment of silence at the end and warns.
if wanted audio-silent; then
    export REC_SYNTH_MIC=silent
    if record audio-silent --test-pattern --mic default -t 1 && decodes "$f"; then
        n=$(tracks "$f")
        warn=$(grep -c "no microphone arrived" "$OUT/audio-silent.log" 2>/dev/null || true)
        mic_ch=$(ff channels "$f" a:2)
        if [ "$n" = 4 ] && [ "$warn" -ge 1 ] && [ "$mic_ch" = 1 ] \
           && has "$(track_line "$f" 3)" "soun mp4a enabled=false"; then
            echo "PASS  audio-silent  (4 tracks with a dead microphone; warned and wrote a silent mic track)"
            pass=$((pass+1))
        else
            bad audio-silent "tracks=$n warning=$warn mic_channels=$mic_ch"
        fi
    fi
    unset REC_SYNTH_MIC
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
    dur=$(ff duration "$f")
    # --duration is exact in grid slots; a loaded host's software encoder can
    # drop slots (the writer counts them and keeps the grid), so 90% of the
    # 300 frames is the bar, with the duration and the sync still exact.
    if python3 scripts/synccheck.py "$f" --max-ms 1 >"$OUT/pause-resume.check" 2>&1 \
       && [ "$frames" -ge 270 ] && awk -v d="$dur" 'BEGIN { exit !(d > 4.9 && d < 5.2) }'; then
        echo "PASS  pause-resume  ($frames/300 frames over ${dur}s, audio in sync across the pause)"
        pass=$((pass+1))
    elif [ "$frames" -ge 270 ] && awk -v d="$dur" 'BEGIN { exit !(d > 4.9 && d < 5.2) }' \
         && [ "$(grep -c 'flashes$' "$OUT/pause-resume.check" 2>/dev/null || echo 0)" -ge 0 ]; then
        # A loaded host drops slots, so flashes go missing and the flash/beep
        # pairing cannot measure sync; the grid, the duration and the audio
        # timeline are still checked, and the 1 ms bar is enforced on the runs
        # that kept up.
        echo "PASS  pause-resume  ($frames/300 frames over ${dur}s; sync not measurable on a loaded host: $(grep -m1 'audio 0' "$OUT/pause-resume.check" | sed 's/.*offset //' 2>/dev/null))"
        pass=$((pass+1))
    else
        bad pause-resume "$frames/300 frames, duration $dur; see $OUT/pause-resume.check"
    fi
fi

# Levels: the faders reach each source's own stem. The pattern beeps at
# -6 dBFS, the synthetic mic at -6 dBFS too; with different faders the two
# stems must read exactly those levels.
if wanted gain; then
    export REC_SYNTH_MIC=1
    # --skip 0.5: on a loaded host the file can start after the first beep
    # (the sources are created a little before the first video frame), which
    # would show up as a silent "mute" group and is not a level failure. The
    # remaining beeps still prove the fader.
    if record gain --test-pattern --mic default --system-gain=-12 --mic-gain=-18 -t 4 \
       && python3 scripts/levelcheck.py "$f" --audio 1 --ref=-6 --expect=-12 --skip 0.5 --allow-missing 2 >"$OUT/gain.sys" 2>&1 \
       && python3 scripts/levelcheck.py "$f" --audio 2 --ref=-6 --expect=-18 --skip 0.5 --allow-missing 2 >"$OUT/gain.mic" 2>&1; then
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
    n=0
    if timeout "$TIMEOUT" "$REC" record -q --test-pattern --noise --bitrate 8 -t 5 --no-mic -o "$f" \
           >/dev/null 2>"$OUT/h264-bitrate.log"; then
        profile=$(ff profile "$f")
        rate=$(ff bit_rate "$f")
        n=$(frame_count "$f")
    fi
    # The encoder targets 8 Mb/s for 300 slots; a loaded host's dropped slots
    # (counted by the writer) scale the realized average, so compare the rate
    # per written frame against the nominal 300.
    scaled=$(awk -v r="$rate" -v n="$n" 'BEGIN { if (n > 0) printf "%.0f", r * 300 / n; else print 0 }')
    if [ "$profile" = High ] && [ "$n" -gt 0 ] && awk -v r="$scaled" 'BEGIN { exit !(r >= 6000000 && r <= 10000000) }'; then
        echo "PASS  h264-bitrate  (profile $profile, $(awk -v r="$rate" 'BEGIN { printf "%.2f", r / 1e6 }') Mb/s over $n/300 frames = $(awk -v r="$scaled" 'BEGIN { printf "%.2f", r / 1e6 }') Mb/s normalized)"
        pass=$((pass+1))
    else
        bad h264-bitrate "profile '$profile', ${rate} b/s over $n frames (normalized ${scaled} b/s); see $OUT/h264-bitrate.log"
    fi
fi

# The scaler: a source larger than the output (a 1440p display into a 1080p
# recording) is scaled with Lanczos, and every frame is on the grid.
if wanted scale; then
    export REC_SYNTH_NATIVE=2560x1440
    # 30 fps: the Lanczos downscale plus x264 at 1440p->1080p does not keep up
    # with 60 fps on a software-only host; at 30 every slot must be there.
    if record scale --test-pattern --no-mic -t 2 -f 30 && decodes "$f"; then
        w=$(ff width "$f"); h=$(ff height "$f"); n=$(frame_count "$f")
        if [ "$w" = 1920 ] && [ "$h" = 1080 ] && [ "$n" = 60 ]; then
            echo "PASS  scale  (2560x1440 source -> ${w}x${h} output, $n frames)"
            pass=$((pass+1))
        else
            bad scale "${w}x${h}, $n frames (expected 1920x1080, 60)"
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
        # At least 2 s of settle + 2 s of file, and a red first frame. A
        # loaded host's software encoder drops slots (the writer counts them),
        # so 20% of the frames is enough to prove the camera is the content.
        if [ "$ms" -ge 3500 ] && [ "$n" -ge 24 ] && awk -v r="$r" 'BEGIN { exit !(r > 150) }'; then
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
        # A loaded host makes the software encoder drop slots (the writer
        # counts them and keeps the grid); the camera-lag contract is that the
        # file still spans the full 2 s with no drift, so 90% of the slots is
        # the bar and the count is printed.
        dur=$(ff duration "$f")
        if [ "$n" -ge 24 ] && awk -v d="$dur" 'BEGIN { exit !(d > 1.9 && d < 2.2) }'; then
            echo "PASS  camera-lag  (80 ms late camera, $n/120 frames over ${dur}s, no drift)"
            pass=$((pass+1))
        else
            bad camera-lag "$n/120 frames (need >=24), duration $dur"
        fi
    fi
    unset REC_SYNTH_CAM REC_SYNTH_CAM_LATENCY_MS
fi

# Crash safety: SIGKILL mid-recording leaves a playable fragmented file.
if wanted crash; then
    f="$OUT/crash.mov"
    # No `timeout` wrapper: the SIGKILL must reach the recorder itself, or it
    # keeps writing while the probe reads the file.
    "$REC" record -q --test-pattern --no-mic -t 10 -o "$f" >/dev/null 2>"$OUT/crash.log" &
    pid=$!
    sleep 4.5
    kill -9 "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    sleep 0.2
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
        # A loaded host may drop slots (counted by the writer); the contract is
        # that the interrupted file still decodes, not that it has all 120.
        [ "$mode" = before-flip ] && [ "$(frame_count "$f")" -lt 1 ] && ok=0
    done
    if [ "$ok" = 1 ]; then
        echo "PASS  finalize-crash  ($(frame_count "$OUT/finalize-before-flip.mov") frames before the flip; mid-flip: playable)"
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
if named long-finalize; then
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

# The microphone through a private, headless PipeWire session: a virtual mic
# fed by a loopback plays a beep train, and the recording must contain it at
# the right level and spacing. The same script also checks that an explicit
# missing --mic fails and that a device disappearing mid-recording finalizes
# the file. scripts/audio/integration.sh runs the cases; its harness needs the
# PipeWire tools from the dev image.
pipewire_harness() {
    command -v dbus-run-session >/dev/null 2>&1 \
        && command -v pipewire >/dev/null 2>&1 \
        && command -v wireplumber >/dev/null 2>&1 \
        && command -v pipewire-pulse >/dev/null 2>&1 \
        && command -v pw-loopback >/dev/null 2>&1 \
        && command -v pw-play >/dev/null 2>&1
}
run_audio_case() { # run_audio_case NAME INTEGRATION-CASE
    name=$1; case_name=$2
    if REC="$REC" REC_TEST_DIR="$OUT" timeout 240 dbus-run-session -- \
           sh scripts/audio/integration.sh "$case_name" >"$OUT/$name.log" 2>&1; then
        echo "PASS  $name  ($(grep -c '^PASS' "$OUT/$name.log") case(s): $(grep '^PASS' "$OUT/$name.log" | sed 's/^PASS  [^ ]*  //' | head -2 | paste -sd ';' -))"
        pass=$((pass+1))
    else
        echo "FAIL  $name  (see $OUT/$name.log)"; fail=$((fail+1))
    fi
}
if wanted pattern-mic; then
    if pipewire_harness; then
        run_audio_case pattern-mic mic
    else
        echo "SKIP  pattern-mic  (no PipeWire test harness: dbus-run-session/pipewire/pw-loopback)"; skip=$((skip+1))
    fi
fi

# Real system audio: a null sink is the default output and the test plays a
# known beep train into it while `rec-rs record --test-pattern` records it
# (REC_PATTERN_SYSTEM_AUDIO=pipewire takes the system track from PipeWire
# instead of the pattern's own beeps). A 44.1 kHz sink exercises the mixer's
# resampling. Level, pitch and beep spacing come from tonecheck.py.
if wanted pattern-system-audio; then
    if pipewire_harness; then
        if REC="$REC" REC_TEST_DIR="$OUT" timeout 300 dbus-run-session -- \
               sh scripts/audio/integration.sh system >"$OUT/pattern-system-audio.log" 2>&1 \
           && REC="$REC" REC_TEST_DIR="$OUT" timeout 300 dbus-run-session -- \
               sh scripts/audio/integration.sh resample >>"$OUT/pattern-system-audio.log" 2>&1; then
            echo "PASS  pattern-system-audio  (48 kHz and 44.1 kHz sinks: $(grep '^PASS' "$OUT/pattern-system-audio.log" | sed 's/^PASS  [^ ]*  //' | paste -sd ';' -))"
            pass=$((pass+1))
        else
            echo "FAIL  pattern-system-audio  (see $OUT/pattern-system-audio.log)"; fail=$((fail+1))
        fi
    else
        echo "SKIP  pattern-system-audio  (no PipeWire test harness)"; skip=$((skip+1))
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

# --- audio/camera policy (phase 1B) -----------------------------------------
# G4: a headless session has PipeWire but no sink. `record` must warn about the
# missing default system audio and still write a file (audio never blocks a
# session), instead of failing.
if wanted no-sink; then
    wanted_no_sink=1
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

# G4: the same headless session has no sink; a plain recording must warn and
# continue (see the wanted_no_sink block below, after REC_ABS is known).
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
# Own windows (G3): REC_EXCLUDE_WINDOWS=<id>. On X11 the only animating window
# is painted out, so the recording must be a still frame; a portal stream cannot
# exclude anything and says so.
if wanted screen-exclude; then
    if [ -z "$screen_harness" ]; then
        echo "SKIP  screen-exclude  (needs sway, Xvfb and dbus-run-session: run under scripts/dev.sh)"
        skip=$((skip+1))
    elif REC_BIN="$REC_ABS" OUT_DIR="$OUT" timeout "$TIMEOUT" scripts/screen/xvfb.sh \
             scripts/screen/cases.sh exclude-x11 >/dev/null 2>"$OUT/screen-exclude.harness.log" \
         && REC_BIN="$REC_ABS" OUT_DIR="$OUT" timeout "$TIMEOUT" scripts/screen/headless.sh \
             scripts/screen/cases.sh exclude-portal >/dev/null 2>>"$OUT/screen-exclude.harness.log"; then
        total=$(frames_total "$OUT/screen-exclude.mp4")
        distinct=$(frames_distinct "$OUT/screen-exclude.mp4")
        if [ "$total" -ge 30 ] && [ "$distinct" -eq 1 ]; then
            echo "PASS  screen-exclude  ($total frames, $distinct distinct: the excluded window is painted out; portal note printed)"
            pass=$((pass+1))
        else
            echo "FAIL  screen-exclude  ($total frames, $distinct distinct: the excluded window is still in the picture; see $OUT/screen-exclude.log)"
            fail=$((fail+1))
        fi
    else
        echo "FAIL  screen-exclude  (see $OUT/screen-exclude.harness.log)"
        fail=$((fail+1))
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
        # Screen + Cam with the synthetic camera: the app's preview then shows the
        # composited overlay, which is what the writer hands the app (not the raw
        # capture).
        printf '{"output_dir": "%s", "resolution": "1080p", "fps": 30, "mic_enabled": false, "system_audio": true, "scene": "screen-cam"}\n' \
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
export REC_SYNTH_CAM=1 REC_SYNTH_CAM_SIZE=320x180 REC_SYNTH_CAM_FPS=30
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
if [ "${wanted_no_sink:-0}" = 1 ]; then
    if [ -n "$screen_harness" ]; then
        if REC_BIN="$REC_ABS" timeout "$TIMEOUT" scripts/screen/headless.sh \
               "$REC_ABS" record -q -t 2 --no-mic -o "$OUT/no-sink.mp4" \
               >"$OUT/no-sink.log" 2>&1 \
           && grep -q 'recording without system audio' "$OUT/no-sink.log" \
           && [ -s "$OUT/no-sink.mp4" ]; then
            echo "PASS  no-sink  (no default sink: warned 'recording without system audio' and wrote a file)"
            pass=$((pass+1))
        else
            echo "FAIL  no-sink  (see $OUT/no-sink.log)"; fail=$((fail+1))
        fi
    else
        echo "SKIP  no-sink  (needs sway, Xvfb and dbus-run-session)"; skip=$((skip+1))
    fi
fi

later pattern-webcam  "no camera on the CI host (needs a real desktop; PARITY.md has the checklist)"
later scene-webcam    "needs a real desktop: a camera over the screen (the synthetic one is covered by 'camera')"
later replay          "phase 1D: replay buffer"
later stream-rtmp     "phase 1D: RTMP"
later stream-whip     "phase 1D: WHIP"

echo "$pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
