#!/bin/sh
# bb runs this in every new worktree before the first turn.
#
# The upstream reference (reference/rec-rs) is a git submodule pinned to the
# upstream commit the port is based on. Fetching it needs network access; when
# that is unavailable the checkout still works, but reading upstream sources
# does not.
set -eu
cd "$(dirname "$0")"

if [ -f .gitmodules ] && git rev-parse --git-dir >/dev/null 2>&1; then
    if ! git submodule update --init --recursive; then
        echo "bb-env-setup: warning: could not fetch the reference/rec-rs submodule (offline?); continuing" >&2
    fi
fi

# The dev container is built on first use by scripts/dev.sh; nothing else needs
# installing on the host.
chmod +x scripts/dev.sh scripts/selftest.sh scripts/synccheck.py scripts/levelcheck.py 2>/dev/null || true
exit 0
