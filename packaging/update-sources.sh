#!/bin/sh
# Regenerate packaging/flatpak/cargo-sources.json from Cargo.lock.
#
#   packaging/update-sources.sh
#
# Every phase adds crates; the Flatpak build has no network, so the vendored
# crate sources must match Cargo.lock exactly. Run this after Cargo.lock
# changes and commit the result with the change that caused it.
#
# It runs the official flatpak-cargo-generator (from the Flathub builder
# image, which is also what packaging/flatpak/build-flatpak.sh uses), as the
# host user so the file in the checkout stays owned by you. It needs network:
# the generator reads crate metadata from crates.io and records each archive's
# URL and sha256.
set -eu

REPO=$(cd "$(dirname "$0")/.." && pwd -P)
IMAGE=${FLATPAK_IMAGE:-ghcr.io/flathub-infra/flatpak-github-actions:gnome-49}
LOCK=${1:-Cargo.lock}
OUT=${2:-packaging/flatpak/cargo-sources.json}

command -v docker >/dev/null 2>&1 || {
    echo "update-sources.sh: docker is required (flatpak-cargo-generator lives in the Flathub image)" >&2
    exit 1
}

if [ -z "${DOCKER_CONFIG:-}" ] && [ -f "${HOME:-/nonexistent}/.docker/config.json" ] &&
   grep -q '"credsStore"[[:space:]]*:[[:space:]]*"desktop' "${HOME}/.docker/config.json" 2>/dev/null &&
   ! command -v docker-credential-desktop.exe >/dev/null 2>&1; then
    DOCKER_CONFIG=${TMPDIR:-/tmp}/rec-rs-linux-docker
    mkdir -p "$DOCKER_CONFIG"
    printf '{"auths":{}}\n' >"$DOCKER_CONFIG/config.json"
    export DOCKER_CONFIG
fi

before=$(sha256sum "$OUT" 2>/dev/null | cut -d' ' -f1 || true)
docker run --rm \
    --user "$(id -u):$(id -g)" \
    -v "$REPO:$REPO" \
    -w "$REPO" \
    -e HOME=${TMPDIR:-/tmp} \
    "$IMAGE" \
    flatpak-cargo-generator -o "$OUT" "$LOCK"

after=$(sha256sum "$OUT" | cut -d' ' -f1)
crates=$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$OUT" 2>/dev/null ||
         grep -c '"type": "archive"' "$OUT")
echo "update-sources.sh: $OUT ($crates entries, sha256 ${after%)})"
if [ "$before" = "$after" ]; then
    echo "update-sources.sh: unchanged (Cargo.lock already vendored)"
else
    echo "update-sources.sh: regenerated (was ${before:-missing})"
fi
