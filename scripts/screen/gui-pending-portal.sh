#!/bin/sh
# scripts/dev.sh scripts/screen/gui-pending-portal.sh
# A private fake portal, NOT headless.sh (which starts the real portal).
# Default observation is 185s; never permit less than the required 180s.
# Keep REC_PORTAL_TIMEOUT unset for the default-timeout regression. A separate
# REC_PORTAL_TIMEOUT=300 run is a positive control, NOT proof the default passes.
set -eu
if [ "${REC_PENDING_SESSION:-}" != 1 ]; then
    exec dbus-run-session -- env REC_PENDING_SESSION=1 sh "$0" "$@"
fi
exec python3 "$(dirname "$0")/gui-pending-portal.py"
