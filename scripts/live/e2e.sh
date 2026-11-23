#!/bin/sh
# The real recorder, end to end: `rec-rs record` with the test pattern writes a
# file, streams to RTMP and WHIP from the same encode, keeps a replay buffer,
# and `rec-rs save-replay` from another shell writes clips while it records.
# A pause/resume in the middle must not disturb any of it. Then `--no-record`
# streams without writing a file.
#
#   scripts/live/e2e.sh
#
# Output goes to ${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}/live/e2e.
set -eu
cd "$(dirname "$0")/../.."
. scripts/live/lib.sh

REC=$(rec_binary)
OUT="$(live_root)/e2e"
TIMEOUT="${REC_TIMEOUT:-180}"
# The self-signed server certificate is the one case the switch is for.
export REC_INSECURE_TLS=1
export REC_STREAM_KEY="${REC_STREAM_KEY:-e2e-key}"

pass=0
fail=0
ok() {
    echo "PASS  $1  ($2)"
    pass=$((pass + 1))
}
bad() {
    echo "FAIL  $1  ($2)"
    fail=$((fail + 1))
}
# sync FILE MAX_MS: "mean" when the file's A/V offset is inside the bound.
sync_of() {
    python3 scripts/synccheck.py "$1" --max-ms "$2" >"$1.check" 2>&1 || {
        echo ""
        return 1
    }
    grep 'audio 0' "$1.check" | head -1 | sed 's/.*offset //; s/,.*//'
}

scripts/live/mediamtx.sh ensure >/dev/null 2>&1 || exit 3
mkdir -p "$OUT"
dir="$OUT/run"
rm -rf "$dir"
mkdir -p "$dir/out" "$dir/rt"
chmod 700 "$dir/rt"
export XDG_RUNTIME_DIR="$dir/rt"
start_server "$dir" "$dir/mediamtx.log"

# --- the recording, two streams and two saves, with a pause in the middle ----
file="$dir/out/rec.mp4"
timeout "$TIMEOUT" "$REC" record --test-pattern --no-mic -r 720p --fps 30 -t 16 \
    --stream rtmp://127.0.0.1:1935/e2e \
    --stream http://127.0.0.1:8889/e2ew/whip \
    --replay 15 -o "$file" >"$dir/out/rec.out" 2>"$dir/out/rec.err" &
rec=$!
sleep 6
"$REC" pause >>"$dir/out/rec.err" 2>&1 && echo "paused" >>"$dir/out/controls" || echo "pause failed" >>"$dir/out/controls"
sleep 2
"$REC" resume >>"$dir/out/rec.err" 2>&1 && echo "resumed" >>"$dir/out/controls" || echo "resume failed" >>"$dir/out/controls"
sleep 3
"$REC" save-replay >>"$dir/out/save.log" 2>&1 && echo "save 1" >>"$dir/out/controls" || echo "save 1 failed" >>"$dir/out/controls"
sleep 2
"$REC" save-replay >>"$dir/out/save.log" 2>&1 && echo "save 2" >>"$dir/out/controls" || echo "save 2 failed" >>"$dir/out/controls"
if wait "$rec"; then
    ok record-exit "the recorder finished cleanly"
else
    bad record-exit "the recorder failed (see $dir/out/rec.err)"
fi
sleep 1

# --- --no-record: streams with no file (same server, still up) --------------
norec="$dir/out/norecord.mp4"
timeout "$TIMEOUT" "$REC" record --test-pattern --no-mic -r 720p --fps 30 -t 8 \
    --no-record --stream rtmp://127.0.0.1:1935/norec -o "$norec" \
    >"$dir/out/norec.out" 2>"$dir/out/norec.err" || true
if [ ! -e "$norec" ]; then
    ok no-record-file "no file was written"
else
    bad no-record-file "$norec exists"
fi
if tr '\r' '\n' <"$dir/out/norec.err" | grep -q 'rtmp://127.0.0.1 live'; then
    ok no-record-live "streamed with no recording file"
else
    bad no-record-live "$(tr '\r' '\n' <"$dir/out/norec.err" | tail -2 | tr '\n' ' ')"
fi
stop_server "$dir"

status=$(tr '\r' '\n' <"$dir/out/rec.err")
# The encode and both destinations were running, with the replay buffer's
# memory in the status line.
if echo "$status" | grep -q 'rtmp://127.0.0.1 live' && echo "$status" | grep -q 'whip://127.0.0.1 live'; then
    ok record-live "both destinations live while recording"
else
    bad record-live "a destination never went live"
fi
if echo "$status" | grep -qE 'replay 1[0-9]s [0-9.]+ MB'; then
    ok record-replay-memory "$(echo "$status" | grep -oE 'replay 1[0-9]s [0-9.]+ MB' | tail -1)"
else
    bad record-replay-memory "no replay memory in the status line"
fi
if grep -q paused "$dir/out/controls" 2>/dev/null && grep -q resumed "$dir/out/controls" 2>/dev/null; then
    ok record-pause "paused and resumed while streaming"
else
    bad record-pause "$(cat "$dir/out/controls" 2>/dev/null | tr '\n' ' ')"
fi

# The file: the full duration despite the pause (—duration counts recorded
# seconds), all tracks, in sync.
if [ -s "$file" ] && "$REC" probe "$file" >"$dir/out/rec.probe" 2>&1; then
    frames=$(ffprobe -v error -select_streams v -count_frames -show_entries stream=nb_read_frames -of csv=p=0 "$file")
    if [ "${frames:-0}" -ge 460 ]; then
        ok record-file "$frames frames, $(basename "$file")"
    else
        bad record-file "$frames frames (a pause must not shorten the file)"
    fi
    if off=$(sync_of "$file" 20); then
        ok record-sync "A/V $off"
    else
        bad record-sync "$(grep 'audio 0' "$file.check" | head -1)"
    fi
else
    bad record-file "no file, or it does not probe"
fi

# The server's recordings: RTMP and WHIP from the same encode.
rtmp_rec=$(newest_recording "$dir/recordings/e2e")
if off=$(sync_of "${rtmp_rec:-/nonexistent}" 25 2>/dev/null); then
    ok record-stream-rtmp "A/V $off"
else
    bad record-stream-rtmp "no usable RTMP recording"
fi
whip_rec=$(newest_recording "$dir/recordings/e2ew")
if off=$(sync_of "${whip_rec:-/nonexistent}" 45 2>/dev/null); then
    ok record-stream-whip "A/V $off"
else
    bad record-stream-whip "no usable WHIP recording"
fi
codec=$(ffprobe -v error -select_streams a -show_entries stream=codec_name -of csv=p=0 "${whip_rec:-/nonexistent}" 2>/dev/null || true)
if [ "$codec" = opus ]; then
    ok record-whip-audio "Opus, as WebRTC requires"
else
    bad record-whip-audio "audio codec '${codec:-none}'"
fi

# The two replay clips written on demand while it recorded.
# The clips land next to the file being recorded (its folder).
clips=$(find "$dir" -name 'replay-*.mp4' -type f 2>/dev/null | sort)
count=$(echo "$clips" | grep -c 'mp4$' || true)
if [ "$count" -eq 2 ]; then
    ok record-saves "two clips written while recording"
else
    bad record-saves "$count clip(s)"
fi
for clip in $clips; do
    dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$clip" 2>/dev/null || echo 0)
    line=$(python3 scripts/synccheck.py "$clip" --max-ms 45 2>&1 | grep 'audio 0' | head -1)
    mean=$(echo "$line" | sed 's/.*offset mean //; s/ ms.*//')
    if awk -v d="$dur" 'BEGIN { exit !(d >= 4.0 && d <= 16.5) }' \
        && [ -n "$mean" ] && awk -v m="$mean" 'BEGIN { exit !(m >= -20 && m <= 20) }'; then
        ok "record-replay-clip" "$(awk -v d="$dur" 'BEGIN { printf "%.1f s", d }'), A/V mean $mean ($(basename "$clip"))"
    else
        bad "record-replay-clip" "$clip: ${dur}s, $line"
    fi
done

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
