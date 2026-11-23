#!/bin/sh
# Build the Rec AppImage and its zsync file.
#
#   packaging/appimage/build-appimage.sh
#
# The build runs in an Ubuntu 24.04 container: the AppImage must start on every
# current distribution, and 24.04 is the oldest base whose PipeWire (1.0) is new
# enough for pipewire-rs 0.10 — Ubuntu 22.04's 0.3.48 lacks spa_video_info_raw
# .flags and spa_meta_first, so libspa 0.10 does not compile there. Its glibc is
# 2.39, which sets the floor: Ubuntu 24.04+, Debian 13+, Fedora 40+ (the base
# containers packaging/appimage/test-appimage.sh uses are newer than that).
# It also carries GStreamer 1.24, whose VA-API plugin is bundled (the driver
# itself stays on the host, see packaging/README.md).
#
# The container carries the GStreamer 1.20 packages whose plugins are bundled
# (x264enc from gst-plugins-ugly, x265enc from gst-plugins-bad, AAC and ProRes
# from gst-libav, Opus from gst-plugins-base), linuxdeploy and appimagetool
# (both pinned by release and sha256 in build-inside.sh). Rust comes from
# rustup, cached in the named volume rec-rs-appimage-cache together with the
# target directory, so a rebuild does not recompile every crate.
#
# Artifacts (never inside the repo):
#   $REC_PKG_OUT/Rec-<version>-x86_64.AppImage
#   $REC_PKG_OUT/Rec-<version>-x86_64.AppImage.zsync
# REC_PKG_OUT defaults to ${XDG_CACHE_HOME:-$HOME/.cache}/rec-rs-linux-packaging.
set -eu

REPO=$(cd "$(dirname "$0")/../.." && pwd -P)
IMAGE=${APPIMAGE_BUILD_IMAGE:-ubuntu:24.04}
OUT=${REC_PKG_OUT:-${XDG_CACHE_HOME:-$HOME/.cache}/rec-rs-linux-packaging}
VOLUME=${APPIMAGE_VOLUME:-rec-rs-appimage-cache}
MEM=${PKG_MEMORY:-6g}
JOBS=${PKG_JOBS:-4}
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO/Cargo.toml" | head -1)

command -v docker >/dev/null 2>&1 || {
    echo "build-appimage.sh: docker is required (the host has no toolchain)" >&2
    exit 1
}
[ -n "$VERSION" ] || {
    echo "build-appimage.sh: could not read the version from Cargo.toml" >&2
    exit 1
}
mkdir -p "$OUT"

if [ -z "${DOCKER_CONFIG:-}" ] && [ -f "${HOME:-/nonexistent}/.docker/config.json" ] &&
   grep -q '"credsStore"[[:space:]]*:[[:space:]]*"desktop' "${HOME}/.docker/config.json" 2>/dev/null &&
   ! command -v docker-credential-desktop.exe >/dev/null 2>&1; then
    DOCKER_CONFIG=${TMPDIR:-/tmp}/rec-rs-linux-docker
    mkdir -p "$DOCKER_CONFIG"
    printf '{"auths":{}}\n' >"$DOCKER_CONFIG/config.json"
    export DOCKER_CONFIG
fi

NAME="rec-rs-appimage-$$"
cleanup() { docker kill "$NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

echo "build-appimage.sh: Rec $VERSION on $IMAGE (jobs $JOBS, memory $MEM)"
exec docker run --rm --init -i --name "$NAME" \
    --memory "$MEM" --memory-swap "$MEM" \
    -v "$REPO:$REPO" \
    -v "$OUT:/out" \
    -v "$VOLUME:/root" \
    -w "$REPO" \
    -e REPO="$REPO" -e OUT=/out -e JOBS="$JOBS" -e VERSION="$VERSION" \
    -e CARGO_BUILD_JOBS="$JOBS" \
    "$IMAGE" sh "$REPO/packaging/appimage/build-inside.sh"
