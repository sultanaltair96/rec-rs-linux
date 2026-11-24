#!/bin/sh
# Run inside headless.sh: remove the recording's exact portal node and verify
# it stops promptly, never negotiates a foreign format, and finalises its MP4.
set -eu
B=${REC_BIN:?} OUT=${OUT_DIR:?}
log="$OUT/screen-disconnect.log"
file="$OUT/screen-disconnect.mp4"
rm -f "$log" "$file"
# Export a real competing source, with the camera's different format. Verify
# its PipeWire registry entry and advertised format, not just the pipeline's
# intended caps, before removing the screen node.
gst-launch-1.0 videotestsrc is-live=true pattern=ball \
    ! video/x-raw,format=YUY2,width=640,height=480,framerate=30/1 \
    ! pipewiresink mode=provide \
      stream-properties='props,node.name=rec-competing-yuy2,media.class=Video/Source' \
    >"$OUT/screen-disconnect-competitor.log" 2>&1 &
competitor=$!
pid=''
trap '[ -z "$pid" ] || kill "$pid" 2>/dev/null || true; kill "$competitor" 2>/dev/null || true; wait "$competitor" 2>/dev/null || true' EXIT HUP INT TERM
find_competitor() {
    pw-dump | python3 -c 'import json,sys
for o in json.load(sys.stdin):
    p = o.get("info", {}).get("props", {})
    if p.get("node.name") == "rec-competing-yuy2" and p.get("media.class") == "Video/Source":
        print(o["id"]); break'
}
n=0
foreign=''
while [ -z "$foreign" ]; do
    kill -0 "$competitor" 2>/dev/null || { echo 'competing video producer exited' >&2; exit 1; }
    foreign=$(find_competitor)
    n=$((n+1)); [ "$n" -lt 100 ] || { echo 'competing node did not appear' >&2; exit 1; }
    [ -n "$foreign" ] || sleep .1
done
pw-cli enum-params "$foreign" EnumFormat >"$OUT/screen-disconnect-competitor-format.log"
grep -q 'VideoFormat:YUY2' "$OUT/screen-disconnect-competitor-format.log"
grep -q 'Rectangle 640x480' "$OUT/screen-disconnect-competitor-format.log"
REC_DEBUG=1 "$B" record -t 60 --no-mic --no-system-audio -o "$file" >"$log" 2>&1 &
pid=$!
n=0
while ! grep -q 'first screen frame delivered' "$log" 2>/dev/null; do
    kill -0 "$pid" 2>/dev/null || { echo 'recorder exited before first frame' >&2; exit 1; }
    n=$((n+1)); [ "$n" -lt 200 ] || { echo 'timed out waiting for frame' >&2; exit 1; }
    sleep .1
done
node=$(awk '/screen stream connected to node/ { print $NF; exit }' "$log")
case "$node" in ''|*[!0-9]*) echo 'missing portal node id' >&2; exit 1;; esac
[ "$node" != "$foreign" ]
sleep 2
pw-cli destroy "$node" >"$OUT/screen-disconnect-destroy.log" 2>&1
n=0
while kill -0 "$pid" 2>/dev/null; do
    n=$((n+1)); [ "$n" -lt 200 ] || { echo 'recorder did not stop after node removal' >&2; exit 1; }
    sleep .1
done
wait "$pid"
pid=''
kill -0 "$competitor"
[ "$(find_competitor)" = "$foreign" ]
grep -q 'screen capture stopped: the shared screen may have gone away' "$log"
[ "$(grep -c 'screen stream format' "$log")" -eq 1 ]
! grep -q 'screen stream format VideoFormat::YUY2' "$log"
"$B" probe "$file" >"$OUT/screen-disconnect-probe.log" 2>&1
ffmpeg -v error -i "$file" -f null -
echo "screen disconnect: node $node removed with YUY2 competitor $foreign present; recorder stopped and file decoded"
