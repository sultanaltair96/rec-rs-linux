#!/usr/bin/env bash
# Opt in with DEV_SMALL_TMPFS=96m scripts/dev.sh bash scripts/media/disk-faults.sh --run.
# Docker owns the disposable mount: no mount privileges or user namespaces needed.
set -euo pipefail
HERE=$(cd -- "$(dirname -- "$0")" && pwd -P)
if [[ ${1:-} != --run ]]; then
    echo 'SKIP disk-faults (opt in: DEV_SMALL_TMPFS=96m scripts/dev.sh bash scripts/media/disk-faults.sh --run)'
    exit 0
fi
for tool in python3 ffprobe ffmpeg timeout findmnt find; do
    command -v "$tool" >/dev/null || { echo "FAIL disk-faults (missing $tool)" >&2; exit 1; }
done
mountpoint=/mnt/rec-small
# Refuse to fill any host filesystem or a larger/shared tmpfs by accident.
[[ $(findmnt -n -o FSTYPE --target "$mountpoint" 2>/dev/null) == tmpfs &&
   $(findmnt -n -o TARGET --target "$mountpoint" 2>/dev/null) == "$mountpoint" &&
   -w $mountpoint ]] || { echo 'FAIL disk-faults (requires DEV_SMALL_TMPFS=96m)' >&2; exit 1; }
python3 - "$mountpoint" <<'PY'
import os, sys
s = os.statvfs(sys.argv[1])
assert 32 * 1024**2 <= s.f_blocks * s.f_frsize <= 128 * 1024**2, 'expected small dedicated tmpfs'
PY
REC=${REC:-${CARGO_TARGET_DIR:-target}/release/rec-rs}
[[ -x $REC ]] || { echo 'Build first: scripts/dev.sh cargo build --release' >&2; exit 1; }
REC=$(realpath "$REC")
base=${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}
mkdir -p "$base"
OUT=$(mktemp -d "$base/disk-faults-XXXXXXXX")
work=$(mktemp -d "$mountpoint/disk-faults-XXXXXXXX")
pid=
cleanup() {
    if [[ -n $pid ]]; then kill -TERM "$pid" 2>/dev/null || :; wait "$pid" 2>/dev/null || :; fi
    # Only our private test directory, never the mount itself.
    rm -rf -- "$work"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
export XDG_RUNTIME_DIR="$OUT/runtime" REC_SETTINGS="$OUT/settings.json"
mkdir -m 700 "$XDG_RUNTIME_DIR"
export REC_MIN_FREE_MB=16 REC_LOW_WATER_MB=8 REC_DISK_CHECK_MS=100
# The historical variable is passed to qtmux in milliseconds.
export REC_FRAGMENT_SECS=1000
echo "Artifacts: $OUT"
# Allocate real pages (not truncate/fallocate) until the requested free space.
fill() {
    python3 - "$work/filler" "$1" <<'PY'
import errno, os, sys
path, reserve = sys.argv[1], int(sys.argv[2])
with open(path, 'wb', buffering=0) as f:
    while True:
        s = os.statvfs(path)
        remaining = s.f_bavail * s.f_frsize - reserve
        if remaining <= 0:
            break
        try:
            f.write(bytes(min(65536, remaining)))
        except OSError as e:
            if e.errno != errno.ENOSPC or reserve:
                raise
            break
s = os.statvfs(path)
print(f'available={s.f_bavail * s.f_frsize} reserve={reserve}')
assert s.f_bavail * s.f_frsize <= reserve
PY
}
now() { python3 -c 'import time; print(time.monotonic_ns())'; }
fail=0
for fault in preflight low-water enospc folder-disappearance; do
    mkdir "$work/output"
    file="$work/output/recording.mov"
    # Rescue output must not leak into the user's normal recording directory.
    export REC_OUTPUT_DIR="$OUT/$fault-rescue"
    mkdir "$REC_OUTPUT_DIR"
    good=1
    ready=0
    injected=0
    watermark=8
    [[ $fault != enospc ]] || watermark=0
    if [[ $fault == preflight ]]; then
        fill 8388608 >"$OUT/$fault.fill.log"
    fi
    REC_LOW_WATER_MB=$watermark timeout --kill-after=2 30 "$REC" record -q \
        --test-pattern --noise --no-mic -r 720p -f 30 --bitrate 8 -t 25 \
        -o "$file" >"$OUT/$fault.log" 2>&1 &
    pid=$!
    if [[ $fault != preflight ]]; then
        for ((i=0; i<150; i++)); do
            if python3 "$HERE/fragments.py" snapshot "$file" "$OUT/$fault.before.json" 2>/dev/null; then
                ready=1; break
            fi
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.1
        done
        started=$(now)
        if ((ready)); then
            case $fault in
                low-water) fill 4194304 >"$OUT/$fault.fill.log" && injected=1 ;;
                enospc) fill 0 >"$OUT/$fault.fill.log" && injected=1 ;;
                folder-disappearance)
                    # No retained hard link: the recorder must rescue its own
                    # open, unlinked file into the isolated REC_OUTPUT_DIR.
                    rm -rf -- "$work/output"
                    [[ ! -e $work/output && ! -e $file ]] && injected=1 ;;
            esac
        fi
    else
        started=$(now)
    fi
    status=0
    wait "$pid" || status=$?
    pid=
    elapsed=$(( ($(now) - started) / 1000000 ))
    echo "$fault recorder exit: $status; stop: ${elapsed}ms" | tee "$OUT/$fault.status"
    [[ $status != 0 && $status != 124 && $status != 137 && $elapsed -lt 2000 ]] || good=0
    # Preserve evidence on the roomy filesystem before releasing filler pages.
    [[ ! -f $file ]] || cp "$file" "$OUT/$fault.raw.mov"
    if [[ $fault == preflight ]]; then
        [[ ! -e $file ]] || good=0
        grep -Eq 'low disk space: [0-9]+ bytes available, 16000000 bytes required; stopping recording to finalize safely' "$OUT/$fault.log" || good=0
    else
        ((ready && injected)) || good=0
        case $fault in
            low-water)
                grep -Eq '^rec: error during recording: low disk space: [0-9]+ bytes available, 8000000 bytes required; stopping recording to finalize safely$' "$OUT/$fault.log" || good=0
                # The watermark case must stop before a genuine resource failure.
                if grep -Fq 'No space left' "$OUT/$fault.log"; then good=0; fi ;;
            enospc)
                grep -Fq 'No space left on the resource.' "$OUT/$fault.log" || good=0
                if grep -Fq 'low disk space:' "$OUT/$fault.log"; then good=0; fi ;;
            folder-disappearance)
                [[ ! -e $work/output ]] || good=0
                mapfile -d '' -t rescue_files < <(find "$REC_OUTPUT_DIR" -type f -print0)
                if ((${#rescue_files[@]} == 1)); then
                    file=${rescue_files[0]}
                    [[ -s $file && ! -L $file ]] || good=0
                    grep -Fq "rec: recording stopped: the output folder $work/output was removed. Saved the first " "$OUT/$fault.log" || good=0
                    grep -Fq " to $file." "$OUT/$fault.log" || good=0
                else
                    echo "Expected exactly one rescued recording in $REC_OUTPUT_DIR; found ${#rescue_files[@]}" >&2
                    good=0
                fi ;;
        esac
        # Validate the recorder's output directly: never reconstruct fragments
        # or decode a test-generated recovered copy to turn corruption into PASS.
        if ! ffprobe -v error -show_streams -show_format -of json "$file" \
            >"$OUT/$fault.probe.json" 2>"$OUT/$fault.probe.log"; then good=0; fi
        if ! ffmpeg -v error -xerror -i "$file" -map 0:v:0 -f null - \
            >"$OUT/$fault.decode.log" 2>&1; then good=0; fi
    fi
    if ((good)); then echo "PASS $fault";
    else echo "FAIL $fault (see $OUT)"; fail=$((fail+1)); fi
    rm -rf -- "$work/output" "$work/filler"
done
echo "disk-faults: $((4-fail)) passed, $fail failed, 0 skipped"
((fail == 0))
