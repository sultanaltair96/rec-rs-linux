#!/bin/sh
# Run a command for packaging/flatpak/test-inside.sh with a D-Bus session and an
# unlocked gnome-keyring, so the Secret Service checks have something to talk to.
#
#   packaging/flatpak/with-session.sh test-inside.sh
#
# A headless container has neither; this starts dbus-run-session and unlocks a
# keyring with an empty login password (the CI convention). When gnome-keyring is
# not available the command still runs — test-inside.sh then skips the Secret
# Service check and says why.
set -eu

if ! command -v dbus-run-session >/dev/null 2>&1; then
    echo "with-session: dbus-run-session is missing; running without a session bus" >&2
    exec sh "$@"
fi

# gnome-keyring and the sandbox both want a runtime dir; a container has no
# logind to create one.
if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
    XDG_RUNTIME_DIR="/run/user/$(id -u)"
    export XDG_RUNTIME_DIR
fi

exec dbus-run-session -- sh -c '
if command -v gnome-keyring-daemon >/dev/null 2>&1; then
    eval "$(printf "\n" | gnome-keyring-daemon --unlock --components=secrets 2>/dev/null)" || true
    dbus-update-activation-environment --all >/dev/null 2>&1 || true
fi
exec sh "$@"' sh "$@"
