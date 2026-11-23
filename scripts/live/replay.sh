#!/bin/sh
# The replay buffer: run the live encode for 20 s with `--replay 15`, ask for a
# clip with `rec-rs save-replay` (the SIGRTMIN+1 path a running recorder uses),
# and check the clip is about 15 s, starts on a keyframe, plays, and is in sync.
#
#   scripts/live/replay.sh
#
# Output goes to ${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}/live/replay.
set -eu
cd "$(dirname "$0")/../.."
. scripts/live/lib.sh

REC=$(rec_binary)
OUT="$(live_root)/replay"
TIMEOUT="${REC_TIMEOUT:-180}"
rm -rf "$OUT"
mkdir -p "$OUT"
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

# 20 s of the test pattern with a 15 s replay window in the background, then a
# `save-replay` from another process, exactly like `rec-rs save-replay` next to
# a running `rec-rs record`.
timeout "$TIMEOUT" "$REC" live-selftest --secs 20 --resolution 720p --fps 30 \
    --replay 15 --output-dir "$OUT" >"$OUT/harness.out" 2>"$OUT/harness.err" &
harness=$!
sleep 17
if "$REC" save-replay >"$OUT/save.out" 2>"$OUT/save.err"; then
    ok replay-signal "\`rec-rs save-replay\` reached the running encode"
else
    bad replay-signal "$(tail -1 "$OUT/save.err")"
fi
# Wait for the clip to be written (the save runs on a background thread).
clip=""
i=0
while [ "$i" -lt 200 ]; do
    clip=$(find "$OUT" -maxdepth 1 -name 'replay-*.mp4' -type f 2>/dev/null | sort | tail -1)
    [ -n "$clip" ] && break
    i=$((i + 1))
    sleep 0.1
done
wait "$harness" || true

if [ -z "$clip" ]; then
    bad replay-clip "no replay file was written"
else
    dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$clip" 2>/dev/null || echo 0)
    if awk -v d="$dur" 'BEGIN { exit !(d >= 14.0 && d <= 17.5) }'; then
        ok replay-length "$(awk -v d="$dur" 'BEGIN { printf "%.1f s", d }') for --replay 15"
    else
        bad replay-length "${dur}s for a 15 s window"
    fi
    if "$REC" probe "$clip" >"$OUT/clip.probe" 2>&1; then
        tracks=$(grep -c '^track' "$OUT/clip.probe")
        ok replay-plays "$tracks tracks, every sample decoded"
    else
        bad replay-plays "$(tail -1 "$OUT/clip.probe")"
    fi
    # The first coded packet must be a sync sample (K__ in ffprobe's flags):
    # a clip starts on a keyframe, not on a delta frame that cannot decode.
    key=$(ffprobe -v error -select_streams v -read_intervals '%+#1' -show_entries packet=flags -of csv=p=0 "$clip" 2>/dev/null | head -1)
    if [ "${key#K}" != "$key" ]; then
        ok replay-keyframe "the clip starts on a keyframe"
    else
        bad replay-keyframe "first frame is not a keyframe"
    fi
    if python3 scripts/synccheck.py "$clip" --max-ms 30 >"$OUT/clip.check" 2>&1; then
        ok replay-sync "$(grep 'audio 0' "$OUT/clip.check" | head -1 | sed 's/.*offset //; s/,.*//')"
    else
        bad replay-sync "$(grep 'audio 0' "$OUT/clip.check" | head -1)"
    fi
fi

# The live status must report what the buffer holds (the app shows it).
if grep -q 'live: replay 1[45-9]\.\?[0-9]*s held' "$OUT/harness.out"; then
    ok replay-memory "$(grep -m1 'live: replay .* held' "$OUT/harness.out" | sed 's/live: //')"
else
    bad replay-memory "no replay memory report"
fi
if grep -q 'dropped 0' "$OUT/harness.out"; then
    ok replay-drops "the encode dropped no frames"
else
    bad replay-drops "the encode dropped frames"
fi

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
