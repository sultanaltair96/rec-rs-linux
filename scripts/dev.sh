#!/bin/sh
# Run a command in the rec-rs-linux dev container.
#
#   scripts/dev.sh cargo build --release
#   scripts/dev.sh cargo test
#   scripts/dev.sh scripts/selftest.sh
#   scripts/dev.sh --clean            # kill leftover containers of this repo
#   scripts/dev.sh                    # interactive shell
#
# The image (tag rec-rs-linux-dev) is built from dev/Dockerfile on first use or
# when the Dockerfile changes. The repo is mounted at the same absolute path,
# the command runs as the host uid/gid, the cargo registry cache lives in the
# named volume rec-rs-linux-cache, and CARGO_TARGET_DIR points at a per-worktree
# directory inside that volume so parallel worktrees never wait on one lock.
#
# A container is always named rec-rs-dev-<pid> and carries the label
# rec-rs-dev=1. It runs with --init and --rm, and this script kills it when it
# exits or is interrupted: killing the docker *client* (a CI timeout, an editor
# stopping a task) leaves the container running otherwise, and the next command
# in the pipeline then hangs forever on a pipe nobody reads.
#
# Anything that can block is additionally run under `timeout` *inside* the
# container (DEV_TIMEOUT, default 30 minutes, 0 = no limit), because a host-side
# timeout can only kill the client.
set -eu

REPO=$(cd "$(dirname "$0")/.." && pwd -P)
IMAGE=rec-rs-linux-dev
VOLUME=rec-rs-linux-cache
DOCKERFILE="$REPO/dev/Dockerfile"
NAME="rec-rs-dev-$$"

command -v docker >/dev/null 2>&1 || {
    echo "dev.sh: docker is required (the host has no Rust toolchain)" >&2
    exit 1
}

# Clean up after ourselves however we leave, including Ctrl-C and `timeout`.
cleanup() { docker kill "$NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

if [ "${1:-}" = "--clean" ]; then
    ids=$(docker ps -q --filter label=rec-rs-dev=1) || true
    [ -n "$ids" ] && docker kill $ids >/dev/null 2>&1 || true
    echo "dev.sh: killed ${ids:-no} leftover container(s)"
    exit 0
fi

# Docker Desktop's credential helper (credsStore: desktop.exe) is not on PATH
# under WSL, and pulling a public base image then fails before it starts. Public
# pulls need no credentials, so fall back to an empty config in that case.
if [ -z "${DOCKER_CONFIG:-}" ] && [ -f "${HOME:-/nonexistent}/.docker/config.json" ] &&
   grep -q '"credsStore"[[:space:]]*:[[:space:]]*"desktop' "${HOME}/.docker/config.json" 2>/dev/null &&
   ! command -v docker-credential-desktop.exe >/dev/null 2>&1; then
    DOCKER_CONFIG=${TMPDIR:-/tmp}/rec-rs-linux-docker
    mkdir -p "$DOCKER_CONFIG"
    printf '{"auths":{}}\n' >"$DOCKER_CONFIG/config.json"
    export DOCKER_CONFIG
fi

# --- image ----------------------------------------------------------------
# The image records the Dockerfile's hash as a label, so it is rebuilt when the
# Dockerfile changes and reused otherwise.
want=$(sha256sum "$DOCKERFILE" | cut -d' ' -f1)
have=$(docker image inspect -f '{{index .Config.Labels "rec-rs-linux.dockerfile"}}' "$IMAGE" 2>/dev/null || true)
if [ "$want" != "$have" ]; then
    echo "dev.sh: building $IMAGE (first run takes a few minutes)..." >&2
    docker build --label "rec-rs-linux.dockerfile=$want" -t "$IMAGE" -f "$DOCKERFILE" "$REPO/dev"
fi

UID_HOST=$(id -u)
GID_HOST=$(id -g)

# The named volume is created by the daemon (root); hand it to the host user
# once, then remember it.
owner=$(docker run --rm --user 0 --entrypoint /bin/sh -v "$VOLUME:/home/dev" "$IMAGE" \
        -c 'cat /home/dev/.owner 2>/dev/null || true')
if [ "$owner" != "$UID_HOST:$GID_HOST" ]; then
    docker run --rm --user 0 --entrypoint /bin/sh -v "$VOLUME:/home/dev" "$IMAGE" -c "
        mkdir -p /home/dev/.cargo /home/dev/.cache/rec-rs-linux
        chown -R $UID_HOST:$GID_HOST /home/dev
        printf '%s' '$UID_HOST:$GID_HOST' > /home/dev/.owner" >/dev/null
fi

# --- target dir: one per worktree, keyed by the repo path -----------------
hash=$(printf '%s' "$REPO" | sha256sum | cut -c1-12)
target="/home/dev/.cache/rec-rs-linux/target-$hash"

# Pass the REC_* knobs the scripts use (test dir, synth failures, debug) and a
# normal terminal environment through to the container.
envs="-e CARGO_TARGET_DIR=$target -e REC_REPO=$REPO"
for name in $(env | sed -n 's/^\(REC_[A-Za-z0-9_]*\)=.*/\1/p'); do
    eval "value=\${$name}"
    envs="$envs -e $name=$value"
done

# A ceiling inside the container: a blocking command (gst-launch, a recorder
# waiting for a device) then dies instead of holding the client until someone
# notices.
limit=${DEV_TIMEOUT:-1800}
if [ "$limit" != "0" ]; then
    set -- timeout -k 10 "$limit" "$@"
fi

args="-i"
if [ -t 0 ] && [ -t 1 ]; then
    args="-it"
fi

# shellcheck disable=SC2086 # $envs is a list of -e NAME=value pairs
status=0
docker run --rm --init $args \
    --name "$NAME" \
    --label rec-rs-dev=1 \
    --user "$UID_HOST:$GID_HOST" \
    --workdir "$REPO" \
    -v "$REPO:$REPO" \
    -v "$VOLUME:/home/dev" \
    $envs \
    -e TERM="${TERM:-dumb}" \
    -e HOME=/home/dev \
    "$IMAGE" \
    ${*:+"$@"} || status=$?
exit "$status"
