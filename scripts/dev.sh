#!/bin/sh
# Run a command in the rec-rs-linux dev container.
#
#   scripts/dev.sh cargo build --release
#   scripts/dev.sh cargo test
#   scripts/dev.sh scripts/selftest.sh
#   scripts/dev.sh --clean            # kill leftover containers of this repo
#   scripts/dev.sh                    # interactive shell
#
# The image is built from dev/Dockerfile and tagged by its content
# (rec-rs-linux-dev:<first 12 hex of the Dockerfile's sha256>), so several
# Dockerfile versions coexist: a worktree with an edited Dockerfile builds its
# own tag instead of rebuilding over — and being rebuilt back by — the others.
# A tag is only ever built when it is missing. The repo is mounted at the same
# absolute path, the command runs as the host uid/gid, the cargo registry cache
# lives in the named volume rec-rs-linux-cache (shared by every tag), and
# CARGO_TARGET_DIR points at a per-worktree directory inside that volume so
# parallel worktrees never wait on one lock.
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
# DEV_SMALL_TMPFS=96m opts into a dedicated bounded /mnt/rec-small tmpfs.
# Its pages count against the container's memory ceiling.
set -eu

small_tmpfs=${DEV_SMALL_TMPFS:-}
if [ -n "$small_tmpfs" ]; then
    case "$small_tmpfs" in *[!0-9mM]* | *[!mM] | m | M) echo 'dev.sh: DEV_SMALL_TMPFS must be 1m to 128m' >&2; exit 1 ;; esac
    size_mb=${small_tmpfs%?}
    case "$size_mb" in '' | *[!0-9]*) echo 'dev.sh: invalid DEV_SMALL_TMPFS' >&2; exit 1 ;; esac
    [ "$size_mb" -ge 1 ] && [ "$size_mb" -le 128 ] || { echo 'dev.sh: DEV_SMALL_TMPFS must be 1m to 128m' >&2; exit 1; }
fi

REPO=$(cd "$(dirname "$0")/.." && pwd -P)
IMAGE_BASE=rec-rs-linux-dev
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
# One tag per Dockerfile content, built once. Parallel worktrees can sit on
# different Dockerfile versions without rebuilding each other's image.
content=$(sha256sum "$DOCKERFILE" | cut -c1-12)
IMAGE="$IMAGE_BASE:$content"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "dev.sh: building $IMAGE (first run takes a few minutes)..." >&2
    docker build --label "rec-rs-linux.dockerfile=$content" -t "$IMAGE" -f "$DOCKERFILE" "$REPO/dev"
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
# One GStreamer registry per image content: the shared volume otherwise keeps a
# registry built by an older image, which hides elements the newer image has
# (a stale registry reported no avdec_h264 after ffmpeg-free was replaced).
envs="-e CARGO_TARGET_DIR=$target -e REC_REPO=$REPO -e GST_REGISTRY=/home/dev/.cache/gstreamer-1.0/registry-$content.bin"
for name in $(env | sed -n 's/^\(REC_[A-Za-z0-9_]*\)=.*/\1/p'); do
    eval "value=\${$name}"
    envs="$envs -e $name=$value"
done
# Cargo/rust tuning from the host, via `-e NAME` (Docker copies the value from
# this process's environment, so flags with spaces survive intact). The paths
# that must stay the container's own are excluded, and so are registry
# credentials: never forward a token into a container.
for name in $(env | sed -n 's/^\(CARGO_[A-Za-z0-9_]*\|RUST[A-Za-z0-9_]*\)=.*/\1/p'); do
    case "$name" in
        CARGO_HOME | RUSTUP_HOME | CARGO_TARGET_DIR | CARGO_REGISTRIES_* | *_TOKEN | *_SECRET | \
        RUSTC | RUSTDOC | RUST_SRC_PATH | *_WRAPPER)
            # Paths that point at the host's toolchain or wrappers would not
            # exist in the container; credentials must never enter it.
            continue ;;
    esac
    envs="$envs -e $name"
done

# A plain `cargo build --release` runs one job per host CPU by default. Inside
# the memory-capped container that many rustc processes (the wgpu/egui graph
# alone is heavy) can exhaust the cgroup and get SIGKILLed. Unless the caller
# set CARGO_BUILD_JOBS, default it from the container's own CPU count and
# cgroup memory limit: min(nproc, memory / 1.5 GB), at least 1.
bootstrap='
if [ -z "${CARGO_BUILD_JOBS:-}" ]; then
    jobs=$(nproc 2>/dev/null || echo 1)
    case "$jobs" in
        "" | *[!0-9]*) jobs=1 ;;
    esac
    [ "$jobs" -lt 1 ] && jobs=1
    mem=$(cat /sys/fs/cgroup/memory.max 2>/dev/null || true)
    if [ -z "$mem" ] || [ "$mem" = max ]; then
        mem=$(cat /sys/fs/cgroup/memory/memory.limit_in_bytes 2>/dev/null || true)
    fi
    case "$mem" in
        "" | max | *[!0-9]*) ;;
        *)
            per=$((mem / 1610612736))
            [ "$per" -lt 1 ] && per=1
            [ "$jobs" -gt "$per" ] && jobs=$per
            ;;
    esac
    export CARGO_BUILD_JOBS="$jobs"
fi
if [ "$#" -eq 0 ]; then
    exec bash
fi
exec "$@"
'

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

# sway's binary carries cap_sys_nice=ep; executing it needs that capability in
# the container's bounding set or every sway invocation fails with EPERM.
caps="--cap-add SYS_NICE"
# A GPU host gives the VA-API plugin (and a future hardware test) a real device;
# without /dev/dri the VA elements simply are not registered (dev/ELEMENTS.md).
if [ -d /dev/dri ]; then
    caps="$caps --device /dev/dri:/dev/dri"
fi

# A memory ceiling per container (DEV_MEMORY, default 4g, 0 = none). Several
# worktrees share one host: without it a runaway recording (one grew to 10 GB)
# makes the kernel OOM killer pick a victim host-wide, and it picked the bb host
# daemon, interrupting every agent. With it, only the offending container dies.
mem=${DEV_MEMORY:-4g}
if [ "$mem" != "0" ]; then
    caps="$caps --memory $mem --memory-swap $mem"
fi

# Keep large build/test scratch files off RAM unless explicitly requested.
# Never use Docker's uncapped tmpfs default (half the host's RAM).
if [ -n "$small_tmpfs" ]; then
    caps="$caps --tmpfs /mnt/rec-small:rw,nosuid,nodev,size=$small_tmpfs,uid=$UID_HOST,gid=$GID_HOST,mode=0700"
fi

# shellcheck disable=SC2086 # $envs is a list of -e NAME=value pairs
status=0
docker run --rm --init $args \
    --name "$NAME" \
    --label rec-rs-dev=1 \
    $caps \
    --user "$UID_HOST:$GID_HOST" \
    --workdir "$REPO" \
    -v "$REPO:$REPO" \
    -v "$VOLUME:/home/dev" \
    $envs \
    -e TERM="${TERM:-dumb}" \
    -e HOME=/home/dev \
    "$IMAGE" \
    /bin/sh -c "$bootstrap" sh ${*:+"$@"} || status=$?
exit "$status"
