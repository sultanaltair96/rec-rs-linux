#!/usr/bin/env bash
# A bounded tmpfs in a NEW user + mount namespace. Never fill the host disk.
# Usage: enospc.sh [COMMAND ARGS...]; command gets REC_FAULT_FULL_DIR.
set -euo pipefail
if [[ ${1:-} != --inside ]]; then
    if ! unshare --user --map-root-user --mount true 2>/dev/null; then
        echo 'SKIP enospc: user/mount namespaces unavailable' >&2
        exit 77
    fi
    export REC_FAULT_PARENT_MNT=$(readlink /proc/self/ns/mnt)
    exec unshare --user --map-root-user --mount "$0" --inside "$@"
fi
shift
[[ -n ${REC_FAULT_PARENT_MNT:-} && $(readlink /proc/self/ns/mnt) != "$REC_FAULT_PARENT_MNT" ]] || {
    echo 'enospc: refusing to mount outside a new namespace' >&2; exit 2;
}
mount --make-rprivate /
root=$(mktemp -d "${TMPDIR:-/tmp}/rec-enospc.XXXXXXXX")
mounted=0
cleanup() {
    if [[ $mounted == 1 ]]; then umount "$root" || return; fi
    rmdir "$root"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mount -t tmpfs -o size=8m,nosuid,nodev,noexec tmpfs "$root"
mounted=1
export REC_FAULT_FULL_DIR=$root
# Verify actual errno, not merely any failed dd/write.
python3 - "$root" <<'PY'
import errno, os, sys
root = sys.argv[1]
try:
    with open(root + '/filler', 'wb', buffering=0) as f:
        while True:
            f.write(b'\0' * 65536)
except OSError as e:
    if e.errno != errno.ENOSPC:
        raise
else:
    raise AssertionError('expected ENOSPC')
try:
    with open(root + '/probe', 'wb', buffering=0) as f:
        f.write(b'x')
except OSError as e:
    assert e.errno == errno.ENOSPC, e
else:
    raise AssertionError('fixture is not full')
print('PASS enospc: 8 MiB private tmpfs returned ENOSPC')
PY
if [[ $# -gt 0 ]]; then "$@"; fi
