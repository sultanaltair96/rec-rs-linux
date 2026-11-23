#!/bin/sh
# Install the built rec.flatpak bundle and exercise it inside a container.
#
#   packaging/flatpak/test-flatpak.sh
#
# The checks themselves are in test-inside.sh, which runs as an ordinary user
# (tester) in a plain Fedora container with only `flatpak` installed from
# Fedora's repositories — deliberately not the Flathub builder image, so the
# bundle is exercised the way a desktop installs it. The bundle is installed
# system-wide as root first: a headless container has no session bus, so
# `flatpak install --user` cannot reach flatpak-session-helper, while running
# the installed app as an ordinary user works exactly as on a desktop. The
# runtime comes from the shared rec-rs-flatpak volume the build filled. Recordings go to the user's
# ~/Videos and `flatpak run` with no subcommand starts the app entry point. The
# container has no display, no audio device and no camera, so the test pattern
# is recorded — the same headless part scripts/selftest.sh covers on the host.
# GUI checks that need a real desktop are listed in packaging/README.md.
#
# Evidence and the recorded files are written to
# ${REC_PKG_OUT:-$HOME/.cache/rec-rs-linux-packaging}/test/.
set -eu

REPO=$(cd "$(dirname "$0")/../.." && pwd -P)
IMAGE=${FLATPAK_TEST_IMAGE:-fedora:43}
OUT=${REC_PKG_OUT:-${XDG_CACHE_HOME:-$HOME/.cache}/rec-rs-linux-packaging}
VOLUME=${FLATPAK_VOLUME:-rec-rs-flatpak}
MEM=${PKG_MEMORY:-8g}

[ -f "$OUT/rec.flatpak" ] || {
    echo "test-flatpak.sh: $OUT/rec.flatpak is missing; run packaging/flatpak/build-flatpak.sh first" >&2
    exit 1
}

rm -rf "$OUT/test"
mkdir -p "$OUT/test"
chmod 0777 "$OUT/test" "$OUT"

if [ -z "${DOCKER_CONFIG:-}" ] && [ -f "${HOME:-/nonexistent}/.docker/config.json" ] &&
   grep -q '"credsStore"[[:space:]]*:[[:space:]]*"desktop' "${HOME}/.docker/config.json" 2>/dev/null &&
   ! command -v docker-credential-desktop.exe >/dev/null 2>&1; then
    DOCKER_CONFIG=${TMPDIR:-/tmp}/rec-rs-linux-docker
    mkdir -p "$DOCKER_CONFIG"
    printf '{"auths":{}}\n' >"$DOCKER_CONFIG/config.json"
    export DOCKER_CONFIG
fi

NAME="rec-rs-flatpak-test-$$"
cleanup() { docker kill "$NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

exec docker run --rm --init -i --name "$NAME" \
    --privileged \
    --memory "$MEM" --memory-swap "$MEM" \
    -v "$REPO:$REPO" \
    -v "$OUT:/out" \
    -v "$VOLUME:/var/lib/flatpak" \
    -w "$REPO" \
    "$IMAGE" \
    sh -c "set -e; dnf -y -q install flatpak util-linux >/dev/null 2>&1; \
           useradd -m tester; \
           su - tester -c 'mkdir -p ~/.config ~/Videos && \
               printf \"XDG_VIDEOS_DIR=\\\"\$HOME/Videos\\\"\\n\" > ~/.config/user-dirs.dirs'; \
           flatpak remote-add --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo; \
           flatpak install -y --noninteractive --reinstall --bundle /out/rec.flatpak; \
           chown -R tester /out; \
           su - tester -c 'sh $REPO/packaging/flatpak/test-inside.sh'"
