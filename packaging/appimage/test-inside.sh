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

ok() { echo "PASS  $1"; }
bad() { echo "FAIL  $1"; }

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
