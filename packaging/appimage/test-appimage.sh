#!/bin/sh
# Run the built Rec AppImage on two clean distributions without GStreamer.
#
#   packaging/appimage/test-appimage.sh
#
# The AppImage is built on Ubuntu 22.04; these checks run it on Fedora 43 and
# Debian 13, neither of which has GStreamer installed, as an ordinary user
# (AppImages are normally run from a user's Downloads folder). The only host
# package installed is PipeWire's client library: the AppImage deliberately
# does not bundle libpipewire, because its ABI has to match the daemon it talks
# to (see packaging/README.md), and every desktop has it. AppImages need FUSE
# to mount themselves; in a container that is unavailable, so the runtime's
# own APPIMAGE_EXTRACT_AND_RUN extracts to /tmp instead — the same bytes, the
# same AppRun, only mounted by extraction rather than by FUSE.
#
# Evidence goes to ${REC_PKG_OUT:-$HOME/.cache/rec-rs-linux-packaging}/test-appimage/.
set -eu

REPO=$(cd "$(dirname "$0")/../.." && pwd -P)
OUT=${REC_PKG_OUT:-${XDG_CACHE_HOME:-$HOME/.cache}/rec-rs-linux-packaging}
MEM=${PKG_MEMORY:-4g}
IMAGES=${APPIMAGE_TEST_IMAGES:-"fedora:43 debian:13"}
LOG=$OUT/test-appimage

APPIMAGE=$(ls "$OUT"/Rec-*-x86_64.AppImage 2>/dev/null | head -1) || true
[ -n "${APPIMAGE:-}" ] || {
    echo "test-appimage.sh: no $OUT/Rec-*-x86_64.AppImage; run packaging/appimage/build-appimage.sh first" >&2
    exit 1
}
IMAGE_NAME=$(basename "$APPIMAGE")

if [ -z "${DOCKER_CONFIG:-}" ] && [ -f "${HOME:-/nonexistent}/.docker/config.json" ] &&
   grep -q '"credsStore"[[:space:]]*:[[:space:]]*"desktop' "${HOME}/.docker/config.json" 2>/dev/null &&
   ! command -v docker-credential-desktop.exe >/dev/null 2>&1; then
    DOCKER_CONFIG=${TMPDIR:-/tmp}/rec-rs-linux-docker
    mkdir -p "$DOCKER_CONFIG"
    printf '{"auths":{}}\n' >"$DOCKER_CONFIG/config.json"
    export DOCKER_CONFIG
fi

rm -rf "$LOG"
mkdir -p "$LOG"

status=0
for image in $IMAGES; do
    name=$(echo "$image" | tr ':/' '__')
    echo "================ $image"
    case "$image" in
        fedora:*)
            setup='dnf -y -q install util-linux pipewire-libs >/dev/null 2>&1'
            ;;
        debian:*)
            setup='apt-get update -qq && apt-get install -y -qq --no-install-recommends libpipewire-0.3-0 util-linux passwd >/dev/null'
            ;;
        *)
            setup='true'
            ;;
    esac
    if docker run --rm --init --memory "$MEM" --memory-swap "$MEM" \
            -v "$OUT:/out:ro" -v "$REPO:/repo:ro" -w /tmp \
            -e IMAGE_NAME="$IMAGE_NAME" \
            "$image" \
            sh -c "$setup
                   useradd -m tester
                   su tester -c 'sh /repo/packaging/appimage/test-inside.sh'" \
            2>&1 | tee "$LOG/$name.log"; then
        echo "RESULT $image: done"
    else
        echo "RESULT $image: FAILED (see $LOG/$name.log)"
        status=1
    fi
done
exit "$status"
