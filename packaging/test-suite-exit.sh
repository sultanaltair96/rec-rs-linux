#!/bin/sh
# Regression test for package-suite exit propagation, without Docker or builds.
set -eu
REPO=$(cd "$(dirname "$0")/.." && pwd -P)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT HUP INT TERM
mkdir -p "$TMP/bin" "$TMP/out"
touch "$TMP/out/Rec-test-x86_64.AppImage" "$TMP/out/rec.flatpak"
cat >"$TMP/bin/docker" <<'EOF'
#!/bin/sh
case " $* " in
    *" fedora:43 "*) image=fedora ;;
    *" debian:13 "*) image=debian ;;
    *) exit 97 ;;
esac
echo "fake $image container"
if [ "${FAIL_IMAGE:-}" = "$image" ]; then
    echo 'RESULT pass=1 fail=1 skip=0'
    exit 1
fi
echo 'RESULT pass=1 fail=0 skip=0'
EOF
chmod +x "$TMP/bin/docker"
export PATH="$TMP/bin:$PATH" REC_PKG_OUT="$TMP/out"

for failing in none fedora debian; do
    export FAIL_IMAGE="$failing"
    rc=0
    sh "$REPO/packaging/appimage/test-appimage.sh" >"$TMP/app-$failing.log" 2>&1 || rc=$?
    if [ "$failing" = none ]; then
        [ "$rc" -eq 0 ] || { echo 'FAIL AppImage success'; exit 1; }
    else
        [ "$rc" -ne 0 ] || { echo "FAIL AppImage $failing failure lost"; exit 1; }
        grep -q "RESULT $failing:43: FAILED\|RESULT $failing:13: FAILED" "$TMP/app-$failing.log" || exit 1
    fi
    # Both distributions must run and retain logs even when the first fails.
    grep -q 'fake fedora container' "$TMP/out/test-appimage/fedora_43.log"
    grep -q 'fake debian container' "$TMP/out/test-appimage/debian_13.log"
    echo "PASS AppImage $failing: exit $rc, both logs retained"
done
for failing in none fedora; do
    export FAIL_IMAGE="$failing"
    rc=0
    sh "$REPO/packaging/flatpak/test-flatpak.sh" >"$TMP/flat-$failing.log" 2>&1 || rc=$?
    if [ "$failing" = none ]; then
        [ "$rc" -eq 0 ] || { echo 'FAIL Flatpak success'; exit 1; }
    else
        [ "$rc" -ne 0 ] || { echo 'FAIL Flatpak failure lost'; exit 1; }
    fi
    echo "PASS Flatpak $failing: exit $rc"
done
