#!/bin/sh
# Start MediaMTX for packaging/flatpak/test-inside.sh (the streaming checks).
#
# Runs in the test container as root, before the checks change to the unprivileged
# user. The server listens on the container's loopback, which the Flatpak shares
# (--share=network), records every path as fMP4, and needs no TLS here: the
# packages are tested for RTMP and WHIP, not RTMPS.
#
# The binary comes from scripts/live/mediamtx.sh ensure (the 1D harness), cached
# outside the repository. If it cannot be downloaded the streaming checks skip
# instead of failing the package.
set -eu

REPO=$(cd "$(dirname "$0")/.." && pwd -P)
DIR=/tmp/mediamtx

rm -rf "$DIR"
mkdir -p "$DIR/recordings"

if ! bin=$(sh "$REPO/scripts/live/mediamtx.sh" ensure 2>"$DIR/ensure.log"); then
    echo "test-mediamtx: could not fetch MediaMTX: $(tail -1 "$DIR/ensure.log" 2>/dev/null)" >&2
    exit 0
fi

cat >"$DIR/mediamtx.yml" <<YAML
logLevel: warn
logDestinations: [stdout]
api: no
metrics: no
pprof: no
playback: no
rtsp: no
rtmp: yes
rtmpAddress: 127.0.0.1:1935
hls: no
srt: no
webrtc: yes
webrtcAddress: 127.0.0.1:8889
webrtcEncryption: no
webrtcAllowOrigin: '*'
webrtcLocalUDPAddress: 127.0.0.1:8189
paths:
  all_others:
    record: yes
    recordPath: $DIR/recordings/%path/%Y-%m-%d_%H-%M-%S-%f
    recordFormat: fmp4
    recordPartDuration: 200ms
    recordSegmentDuration: 1h
    recordDeleteAfter: 0s
YAML

( cd "$DIR" && "$bin" "$DIR/mediamtx.yml" >"$DIR/mediamtx.log" 2>&1 & )
# Wait for both listeners before the recorder starts.
i=0
while [ "$i" -lt 50 ]; do
    if (exec 3<>/dev/tcp/127.0.0.1/1935) 2>/dev/null; then
        exec 3>&-
        break
    fi
    i=$((i + 1))
    sleep 0.1
done
: >"$DIR/.started"
echo "test-mediamtx: started $(basename "$bin") (RTMP 1935, WHIP 8889)"
