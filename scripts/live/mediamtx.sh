#!/bin/sh
# MediaMTX for the live tests: one binary that speaks RTMP, RTMPS (self-signed
# certificate) and WHIP, and records every path so the checks can measure what a
# real server received.
#
#   scripts/live/mediamtx.sh ensure          # download the release into the cache
#   scripts/live/mediamtx.sh serve DIR       # generate DIR/mediamtx.yml + cert, exec the server
#
# Nothing is written into the repo: the binary lives in
# ${REC_LIVE_CACHE:-$HOME/.cache/rec-rs-live} and each run's config, certificate
# and recordings live in DIR (the self-test's output folder).
#
# Adapted from rec-rs scripts/selftest.sh (MIT OR Apache-2.0), whose streaming
# cases ran against `brew install mediamtx`.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd -P)
CACHE="${REC_LIVE_CACHE:-$HOME/.cache/rec-rs-live}"
VERSION="${REC_MEDIAMTX_VERSION:-v1.21.4}"

arch() {
    case "$(uname -m)" in
        x86_64 | amd64) echo amd64 ;;
        aarch64 | arm64) echo arm64 ;;
        *) uname -m ;;
    esac
}

ensure() {
    dir="$CACHE/mediamtx-$VERSION-$(arch)"
    bin="$dir/mediamtx"
    if [ -x "$bin" ]; then
        echo "$bin"
        return 0
    fi
    mkdir -p "$CACHE"
    url="https://github.com/bluenviron/mediamtx/releases/download/$VERSION/mediamtx_${VERSION}_linux_$(arch).tar.gz"
    echo "mediamtx: downloading $url" >&2
    tmp="$CACHE/mediamtx-$VERSION.tgz"
    if ! curl -fsSL --retry 2 -o "$tmp" "$url"; then
        echo "mediamtx: download failed" >&2
        return 1
    fi
    mkdir -p "$dir"
    if ! tar -xzf "$tmp" -C "$dir"; then
        echo "mediamtx: unpacking failed" >&2
        return 1
    fi
    rm -f "$tmp"
    [ -x "$bin" ] || {
        echo "mediamtx: $bin is missing after unpacking" >&2
        return 1
    }
    echo "$bin"
}

# A self-signed certificate for RTMPS, built from scripts/live/mkcert.c with the
# image's gcc and libcrypto (the image has no openssl CLI).
cert() {
    dir=$1
    [ -s "$dir/server.crt" ] && [ -s "$dir/server.key" ] && return 0
    bin="$CACHE/mkcert"
    if [ ! -x "$bin" ]; then
        mkdir -p "$CACHE"
        gcc -O2 -o "$bin" "$HERE/mkcert.c" -lcrypto || {
            echo "mediamtx: could not build mkcert (gcc + openssl-devel are needed)" >&2
            return 1
        }
    fi
    "$bin" "$dir/server.crt" "$dir/server.key"
}

serve() {
    dir=$1
    mkdir -p "$dir/recordings"
    # MediaMTX writes a certificate of its own (auto.crt/auto.key) if the
    # configured ones are missing: run it in the run's own folder so it can
    # never drop files into the repo.
    cd "$dir"
    cert "$dir"
    cat >"$dir/mediamtx.yml" <<EOF
logLevel: info
logDestinations: [stdout]
api: no
metrics: no
pprof: no
playback: no
rtsp: no
rtmp: yes
rtmpAddress: 127.0.0.1:1935
rtmpEncryption: optional
rtmpsAddress: 127.0.0.1:1936
rtmpServerKey: $dir/server.key
rtmpServerCert: $dir/server.crt
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
    recordPath: $dir/recordings/%path/%Y-%m-%d_%H-%M-%S-%f
    recordFormat: fmp4
    recordPartDuration: 200ms
    recordSegmentDuration: 1h
    recordDeleteAfter: 0s
EOF
    exec "$(ensure)" "$dir/mediamtx.yml"
}

case "${1:-}" in
    ensure) ensure ;;
    serve) shift; serve "$@" ;;
    *)
        echo "usage: $0 ensure | serve DIR" >&2
        exit 2
        ;;
esac
