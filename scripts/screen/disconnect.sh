#!/bin/sh
# Run inside headless.sh: remove the recording's exact portal node and verify
# it stops promptly, never negotiates a foreign format, and finalises its MP4.
set -eu
B=${REC_BIN:?} OUT=${OUT_DIR:?}
log="$OUT/screen-disconnect.log"
file="$OUT/screen-disconnect.mp4"
rm -f "$log" "$file"
REC_DEBUG=1 "$B" record -t 60 --no-mic --no-system-audio -o "$file" >"$log" 2>&1 &
pid=$!
trap 'kill "$pid" 2>/dev/null || true' EXIT HUP INT TERM
n=0
while ! grep -q 'first screen frame delivered' "$log" 2>/dev/null; do
    kill -0 "$pid" 2>/dev/null || { echo 'recorder exited before first frame' >&2; exit 1; }
    n=$((n+1)); [ "$n" -lt 200 ] || { echo 'timed out waiting for frame' >&2; exit 1; }
    sleep .1
done
node=$(awk '/screen stream connected to node/ { print $NF; exit }' "$log")
case "$node" in ''|*[!0-9]*) echo 'missing portal node id' >&2; exit 1;; esac
sleep 2
pw-cli destroy "$node" >"$OUT/screen-disconnect-destroy.log" 2>&1
n=0
while kill -0 "$pid" 2>/dev/null; do
    n=$((n+1)); [ "$n" -lt 200 ] || { echo 'recorder did not stop after node removal' >&2; exit 1; }
    sleep .1
done
wait "$pid"
trap - EXIT HUP INT TERM
grep -q 'screen capture stopped:' "$log"
[ "$(grep -c 'screen stream format' "$log")" -eq 1 ]
"$B" probe "$file" >"$OUT/screen-disconnect-probe.log" 2>&1
ffmpeg -v error -i "$file" -f null -
echo "screen disconnect: node $node removed, recorder stopped and file decoded"
