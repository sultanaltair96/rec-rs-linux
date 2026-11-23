#!/bin/sh
# Build rec.flatpak from packaging/flatpak/io.github.sultanaltair96.Rec.yml.
#
#   packaging/flatpak/build-flatpak.sh                 # build + bundle
#   packaging/flatpak/build-flatpak.sh --shell         # interactive shell in the builder
#
# Everything happens in a container: the host has no flatpak, no flatpak-builder
# and no sudo. The image is the official Flathub builder base, which already
# carries flatpak 1.18, flatpak-builder and the GNOME 49 runtime the manifest
# asks for; the named volume rec-rs-flatpak keeps runtimes and the build cache
# between runs.
#
# flatpak-builder needs bubblewrap inside docker, so the container runs
# privileged with a memory ceiling. Nothing of this runs on the host and the
# repository is only read (flatpak-builder copies sources into the build dir).
#
# Artifacts (never inside the repo):
#   $REC_PKG_OUT/rec.flatpak          single-file bundle, installable with
#                                     `flatpak install --user rec.flatpak`
#   $REC_PKG_OUT/repo/                local OSTree repository
#   $REC_PKG_OUT/build-dir/           flatpak-builder work tree
# REC_PKG_OUT defaults to ${XDG_CACHE_HOME:-$HOME/.cache}/rec-rs-linux-packaging.
set -eu

REPO=$(cd "$(dirname "$0")/../.." && pwd -P)
IMAGE=${FLATPAK_IMAGE:-ghcr.io/flathub-infra/flatpak-github-actions:gnome-49}
OUT=${REC_PKG_OUT:-${XDG_CACHE_HOME:-$HOME/.cache}/rec-rs-linux-packaging}
VOLUME=${FLATPAK_VOLUME:-rec-rs-flatpak}
MEM=${PKG_MEMORY:-8g}
JOBS=${PKG_JOBS:-8}

command -v docker >/dev/null 2>&1 || {
    echo "build-flatpak.sh: docker is required (the host has no flatpak)" >&2
    exit 1
}
mkdir -p "$OUT/repo" "$OUT/build-dir"

# Docker Desktop's credential helper is not on PATH under WSL; public pulls
# need no credentials, so fall back to an empty config (same workaround as
# scripts/dev.sh).
if [ -z "${DOCKER_CONFIG:-}" ] && [ -f "${HOME:-/nonexistent}/.docker/config.json" ] &&
   grep -q '"credsStore"[[:space:]]*:[[:space:]]*"desktop' "${HOME}/.docker/config.json" 2>/dev/null &&
   ! command -v docker-credential-desktop.exe >/dev/null 2>&1; then
    DOCKER_CONFIG=${TMPDIR:-/tmp}/rec-rs-linux-docker
    mkdir -p "$DOCKER_CONFIG"
    printf '{"auths":{}}\n' >"$DOCKER_CONFIG/config.json"
    export DOCKER_CONFIG
fi

NAME="rec-rs-flatpak-$$"
cleanup() { docker kill "$NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

# The Flathub base image is created for the GitHub action, so it has no
# systemd and no flathub remote in the fresh volume; both are set up here.
# flatpak-builder's own jobs flag keeps x265, meson and cargo inside the
# memory ceiling.
BUILD="set -eu
flatpak remote-add --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak install -y --noninteractive flathub org.freedesktop.Sdk.Extension.rust-stable//25.08
cd '$REPO/packaging/flatpak'
flatpak-builder --repo=/out/repo --force-clean --disable-rofiles-fuse --jobs=$JOBS \
    --install-deps-from=flathub --state-dir=/out/builder-state \
    /out/build-dir io.github.sultanaltair96.Rec.yml
flatpak build-bundle --arch=x86_64 \
    --runtime-repo=https://flathub.org/repo/flathub.flatpakrepo \
    /out/repo /out/rec.flatpak io.github.sultanaltair96.Rec
ls -l /out/rec.flatpak"

if [ "${1:-}" = "--shell" ]; then
    set -- sh
else
    set -- sh -c "$BUILD"
fi

# shellcheck disable=SC2086 # $set is a command list
exec docker run --rm --init -i --name "$NAME" \
    --privileged \
    --memory "$MEM" --memory-swap "$MEM" \
    -v "$REPO:$REPO" \
    -v "$OUT:/out" \
    -v "$VOLUME:/var/lib/flatpak" \
    -w "$REPO/packaging/flatpak" \
    -e FLATPAK_BUILDER_N_JOBS="$JOBS" \
    "$IMAGE" "$@"
