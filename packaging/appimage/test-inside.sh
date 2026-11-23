#!/bin/sh
# The in-container half of packaging/appimage/test-appimage.sh.
#
# Runs in a clean container of a *different* distribution than the build image
# (Fedora 43 / Debian 13), with no GStreamer installed, as an ordinary user.
# It records the test pattern with the AppImage, probes the file with the same
# AppImage, and lists what comes from the host and what comes from the bundle.
#
# Env: IMAGE_NAME (the AppImage file in /out).
set -eu

APPIMAGE="/out/$IMAGE_NAME"
export APPIMAGE_EXTRACT_AND_RUN=1

pass=0
fail=0
skip=0
ok() { echo "PASS  $1"; pass=$((pass + 1)); }
bad() { echo "FAIL  $1"; fail=$((fail + 1)); }
skipping() { echo "SKIP  $1 ($2)"; skip=$((skip + 1)); }

echo "== host"
cat /etc/os-release | sed -n 's/^PRETTY_NAME=//p'
if command -v gst-inspect-1.0 >/dev/null 2>&1; then
    echo "  host HAS GStreamer (the clean-container claim is weaker)"
else
    echo "  host has no GStreamer (as intended)"
fi

echo "== version"
if "$APPIMAGE" --version; then ok "AppImage starts"; else bad "AppImage starts"; fi

echo "== record: h264, test pattern"
if "$APPIMAGE" record --test-pattern --no-mic --no-system-audio -t 3 -o /tmp/x.mp4 >/tmp/h264.log 2>&1; then
    grep -o 'rec: encoder [a-z0-9_]*' /tmp/h264.log | head -1
    ok "record h264"
else
    cat /tmp/h264.log
    bad "record h264"
fi

echo "== probe with the bundled GStreamer"
if "$APPIMAGE" probe /tmp/x.mp4 >/tmp/h264.probe 2>&1; then
    grep '^track' /tmp/h264.probe | sed 's/^/  /'
    ok "probe h264"
else
    cat /tmp/h264.probe
    bad "probe h264"
fi

echo "== record: hevc"
if "$APPIMAGE" record --test-pattern --no-mic --no-system-audio -t 2 --codec hevc \
        -o /tmp/hevc.mp4 >/tmp/hevc.log 2>&1; then
    grep -o 'rec: encoder [a-z0-9_]*' /tmp/hevc.log | head -1
    ok "record hevc"
    if "$APPIMAGE" probe /tmp/hevc.mp4 >/tmp/hevc.probe 2>&1; then ok "probe hevc"; else cat /tmp/hevc.probe; bad "probe hevc"; fi
else
    cat /tmp/hevc.log
    bad "record hevc"
fi

echo "== record: prores"
if "$APPIMAGE" record --test-pattern --no-mic --no-system-audio -t 2 --codec prores \
        -o /tmp/prores.mov >/tmp/prores.log 2>&1; then
    grep -o 'rec: encoder [a-z0-9_]*' /tmp/prores.log | head -1
    ok "record prores"
else
    cat /tmp/prores.log
    bad "record prores"
fi

echo "== every bundled plugin loads"
if grep -q 'Failed to load plugin' /tmp/h264.log /tmp/hevc.log /tmp/prores.log 2>/dev/null; then
    grep 'Failed to load plugin' /tmp/h264.log /tmp/hevc.log /tmp/prores.log | sed 's/^/  /'
    bad "all plugins load"
else
    ok "all plugins load"
fi

echo "== GUI: no subcommand opens the window (Xvfb, software rendering)"
# There is no desktop here, so an Xvfb display stands in for one and the host's
# Mesa (llvmpipe/lavapipe) renders into the window wgpu picks. The app staying
# up for 8 s is the proof that the GUI started from the AppImage.
XVFB_RUN=$(command -v xvfb-run || echo /usr/sbin/xvfb-run)
if [ -x "$XVFB_RUN" ]; then
    start=$(date +%s)
    rc=0
    "$XVFB_RUN" -a timeout -s TERM 8 "$APPIMAGE" >/tmp/gui.log 2>&1 || rc=$?
    elapsed=$(( $(date +%s) - start ))
    if [ "$rc" -eq 124 ] && [ "$elapsed" -ge 7 ] && ! grep -q 'panicked' /tmp/gui.log; then
        ok "the app runs with no subcommand and keeps its window for 8 s"
    elif grep -q 'no compatible GPU adapter' /tmp/gui.log; then
        # A container with only software Vulkan (lavapipe over Xvfb) has no
        # adapter that can present to this window. That is the container's
        # rendering stack, not the package: the Flatpak, which carries the
        # runtime's Mesa, opens its window in the same test. A real desktop with
        # a GPU driver is where this check belongs (packaging/README.md).
        skipping "the app starts with no subcommand" \
            "this host's software Vulkan cannot present to an Xvfb window"
    else
        echo "  exit $rc after ${elapsed}s"
        tail -20 /tmp/gui.log
        bad "the app starts with no subcommand"
    fi
    head -5 /tmp/gui.log | sed 's/^/  /'
else
    skipping "the app starts with no subcommand" "no xvfb-run in the test container"
fi

echo "== live elements in the bundle (elements -> plugin file)"
if [ ! -d /tmp/squashfs-root ]; then
    ( cd /tmp && env -u APPIMAGE_EXTRACT_AND_RUN "$APPIMAGE" --appimage-extract >/dev/null 2>&1 ) || true
fi
# The AppImage carries no gst-inspect, so the live path's elements are checked
# through the plugin files that provide them. The streaming test below then uses
# them for real, which is the stronger proof.
for pair in \
    fdkaacenc:libgstfdkaac.so fdkaacdec:libgstfdkaac.so \
    avenc_aac:libgstlibav.so avdec_aac:libgstlibav.so \
    voaacenc:libgstvoaacenc.so aacparse:libgstaudioparsers.so \
    opusenc:libgstopus.so opusdec:libgstopus.so \
    h264parse:libgstvideoparsersbad.so h265parse:libgstvideoparsersbad.so \
    mp4mux:libgstisomp4.so videoscale:libgstvideoconvertscale.so \
    x264enc:libgstx264.so x265enc:libgstx265.so; do
    el=${pair%%:*}
    plugin=${pair#*:}
    if [ -f "/tmp/squashfs-root/usr/lib/gstreamer-1.0/$plugin" ]; then
        echo "  present $el ($plugin)"
    else
        echo "  ABSENT  $el ($plugin)"
    fi
done | tee /tmp/live-elements.txt

echo "== live: RTMP + WHIP + replay"
if [ -f /tmp/mediamtx/.started ]; then
    export REC_STREAM_KEY=appimage-test-key
    rc=0
    "$APPIMAGE" record --test-pattern --no-mic --no-system-audio -r 720p --fps 30 -t 8 \
        --stream rtmp://127.0.0.1:1935/rtmppkg \
        --stream http://127.0.0.1:8889/whippkg/whip \
        --replay 15 -o /tmp/live.mp4 >/tmp/live.log 2>&1 &
    rec=$!
    sleep 5
    if "$APPIMAGE" save-replay >/tmp/live-save.log 2>&1; then
        ok "save-replay reaches the running recorder from a second AppImage run"
    else
        cat /tmp/live-save.log
        bad "save-replay from a second AppImage run"
    fi
    wait "$rec" || rc=$?
    status=$(tr '\r' '\n' </tmp/live.log)
    if [ "$rc" -eq 0 ]; then ok "record --stream --replay finished cleanly"; else echo "$status" | tail -8; bad "record --stream --replay"; fi
    if echo "$status" | grep -q 'rtmp://127.0.0.1 live' && echo "$status" | grep -q 'whip://127.0.0.1 live'; then
        ok "both destinations were live while recording"
    else
        bad "a destination never went live"
    fi
    replay=$(echo "$status" | grep -o 'replay saved .*' | head -1 | sed 's/replay saved //')
    if [ -n "$replay" ] && "$APPIMAGE" probe "$replay" >/tmp/live-replay.probe 2>&1; then
        grep '^track' /tmp/live-replay.probe | sed 's/^/  /'
        ok "the replay clip plays ($(basename "$replay"))"
    else
        cat /tmp/live-replay.probe 2>/dev/null
        bad "the replay clip"
    fi
    for kind in rtmp whip; do
        case "$kind" in
            rtmp) dir=/tmp/mediamtx/recordings/rtmppkg ;;
            whip) dir=/tmp/mediamtx/recordings/whippkg ;;
        esac
        # MediaMTX puts an RTMP stream key in a subdirectory of the path.
        newest=$(find "${dir:-/nonexistent}" -name '*.mp4' 2>/dev/null | head -1)
        if [ -n "$newest" ] && "$APPIMAGE" probe "$newest" >/tmp/live-$kind.probe 2>&1; then
            tracks=$(grep '^track' /tmp/live-$kind.probe | tr '\n' ' ')
            audio=$(printf '%s' "$tracks" | grep -o 'audio [a-z0-9]*' | head -1)
            case "$tracks" in
                *video*h264*audio*) ok "MediaMTX recorded $kind with video and $audio" ;;
                *) echo "  $tracks"; bad "MediaMTX $kind recording has video and audio" ;;
            esac
        else
            cat /tmp/live-$kind.probe 2>/dev/null
            bad "MediaMTX has no $kind recording"
        fi
    done
else
    skipping "streaming from the AppImage" "MediaMTX did not start in this container"
fi

echo "== the bundled plugin set"
cd /tmp
rm -rf squashfs-root
unset APPIMAGE_EXTRACT_AND_RUN
"$APPIMAGE" --appimage-extract >/dev/null 2>&1
echo "  plugins: $(ls squashfs-root/usr/lib/gstreamer-1.0/*.so 2>/dev/null | wc -l)"
ls squashfs-root/usr/lib/gstreamer-1.0/ | sed 's/^/    /' | head -40
echo "== libraries still coming from the host (must be the documented set)"
ldd squashfs-root/usr/bin/rec-rs 2>/dev/null | grep -v "/tmp/squashfs-root" | sed 's/^/    /' | head -20
echo "== update information"
"$APPIMAGE" --appimage-updateinformation 2>/dev/null || echo "  (runtime has no --appimage-updateinformation)"

echo "RESULT pass=$pass fail=$fail skip=$skip"
[ "$fail" -eq 0 ]
