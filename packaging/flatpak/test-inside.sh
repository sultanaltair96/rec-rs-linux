#!/bin/sh
# The in-container half of packaging/flatpak/test-flatpak.sh.
#
# Runs as an ordinary user (tester, HOME=/home/tester) inside the Flathub
# builder image, so `$HOME/Videos`, `flatpak --user` and the sandbox's home
# directory behave the way they do on a desktop. /out is the shared artifact
# directory; every result is also written there as evidence.
#
# Not meant to be run on the host: use packaging/flatpak/test-flatpak.sh.
set -eu

APP=io.github.sultanaltair96.Rec
OUT=/out/test
pass=0
fail=0
skip=0

ok() { echo "PASS  $1"; pass=$((pass + 1)); }
bad() { echo "FAIL  $1"; fail=$((fail + 1)); }
skipping() { echo "SKIP  $1 ($2)"; skip=$((skip + 1)); }

echo "== the installed app"
flatpak list | grep "$APP" | tee "$OUT/installed.txt"

echo "== encoders and capture elements inside the sandbox"
{
    for el in x264enc x265enc openh264enc avenc_aac fdkaacenc avenc_prores_ks opusenc \
              pipewiresrc appsrc appsink videoconvert videoscale mp4mux qtmux qtdemux \
              h264parse h265parse avdec_h264 avdec_h265; do
        if flatpak run --command=gst-inspect-1.0 "$APP" "$el" >/dev/null 2>&1; then
            echo "  present $el"
        else
            echo "  ABSENT  $el"
        fi
    done
} | tee "$OUT/gst-elements.txt"

echo "== record: h264 through the CLI, output in ~/Videos"
mkdir -p "$HOME/Videos"
if flatpak run "$APP" record --test-pattern --no-mic --no-system-audio -t 3 \
        -o "$HOME/Videos/x.mp4" >"$OUT/h264.log" 2>&1; then
    grep -o 'rec: encoder [a-z0-9_]*' "$OUT/h264.log" | head -1
    ok "record h264"
else
    cat "$OUT/h264.log"
    bad "record h264"
fi
if ls -l "$HOME/Videos/x.mp4" >"$OUT/h264.ls" 2>&1; then
    ok "recording lands in ~/Videos"
    cat "$OUT/h264.ls"
else
    cat "$OUT/h264.ls"
    bad "recording lands in ~/Videos"
fi

echo "== probe inside the sandbox (GStreamer decodes every sample)"
if flatpak run --command=rec-rs "$APP" probe "$HOME/Videos/x.mp4" >"$OUT/h264.probe" 2>&1; then
    grep '^track' "$OUT/h264.probe" | sed 's/^/  /'
    ok "probe h264"
else
    cat "$OUT/h264.probe"
    bad "probe h264"
fi

echo "== default output folder (no -o)"
if flatpak run --command=rec-rs "$APP" record --test-pattern --no-mic --no-system-audio -t 2 \
        >"$OUT/default.log" 2>&1; then
    out=$(tail -1 "$OUT/default.log")
    case "$out" in
        "$HOME"/Videos/rec-*.mov) ok "default output: $out" ;;
        *)
            echo "  got: $out"
            bad "default output"
            ;;
    esac
else
    cat "$OUT/default.log"
    bad "default output"
fi

echo "== record: hevc (x265enc built by this manifest)"
if flatpak run --command=rec-rs "$APP" record --test-pattern --no-mic --no-system-audio \
        -t 2 --codec hevc -o "$HOME/Videos/hevc.mp4" >"$OUT/hevc.log" 2>&1; then
    grep -o 'rec: encoder [a-z0-9_]*' "$OUT/hevc.log" | head -1
    ok "record hevc"
    if flatpak run --command=rec-rs "$APP" probe "$HOME/Videos/hevc.mp4" >"$OUT/hevc.probe" 2>&1; then
        ok "probe hevc"
    else
        cat "$OUT/hevc.probe"
        bad "probe hevc"
    fi
else
    cat "$OUT/hevc.log"
    bad "record hevc"
fi

echo "== record: prores (the runtime's avenc_prores_ks)"
if flatpak run --command=rec-rs "$APP" record --test-pattern --no-mic --no-system-audio \
        -t 2 --codec prores -o "$HOME/Videos/prores.mov" >"$OUT/prores.log" 2>&1; then
    grep -o 'rec: encoder [a-z0-9_]*' "$OUT/prores.log" | head -1
    ok "record prores"
else
    cat "$OUT/prores.log"
    bad "record prores"
fi

echo "== GUI entry point (no subcommand)"
if flatpak run "$APP" >"$OUT/gui.log" 2>&1; then
    bad "no subcommand should fail before phase 1E"
else
    if grep -q 'graphical app is not implemented yet (phase 1E)' "$OUT/gui.log"; then
        ok "no subcommand starts rec-rs and reports the app phase"
    else
        cat "$OUT/gui.log"
        bad "no subcommand behaviour"
    fi
fi

echo "== settings live in the sandbox's XDG config dir"
if flatpak run --command=sh "$APP" -c '
        set -e
        case "$XDG_CONFIG_HOME" in
            "$HOME"/.var/app/io.github.sultanaltair96.Rec/*) ;;
            *) echo "XDG_CONFIG_HOME=$XDG_CONFIG_HOME is not the app dir"; exit 1 ;;
        esac
        mkdir -p "$XDG_CONFIG_HOME/rec-rs"
        printf "{\"output_dir\":\"%s\"}\n" "$HOME/Videos" > "$XDG_CONFIG_HOME/rec-rs/settings.json"
        cat "$XDG_CONFIG_HOME/rec-rs/settings.json"'; then
    ok "settings path is writable and private to the app"
    ls -l "$HOME/.var/app/io.github.sultanaltair96.Rec/config/rec-rs/settings.json" \
        >"$OUT/settings.ls" 2>&1 && ok "settings file is in ~/.var/app (on the host)"
else
    bad "settings path"
fi

echo "== the exported desktop actions run through flatpak"
exported="$(flatpak info --show-location "$APP")/export/share/applications/$APP.desktop"
if grep -q 'flatpak run.*gui --start-recording' "$exported" &&
   grep -q 'flatpak run.*gui --settings' "$exported"; then
    ok "Start Recording and Settings actions are rewritten for flatpak run"
else
    cat "$exported"
    bad "desktop actions"
fi

echo "== desktop integration inside the sandbox"
if flatpak run --command=sh "$APP" -c \
        'ls /app/share/applications/io.github.sultanaltair96.Rec.desktop
         ls /app/share/metainfo/io.github.sultanaltair96.Rec.metainfo.xml
         ls -d /app/share/icons/hicolor/*/apps/io.github.sultanaltair96.Rec.png' \
        >"$OUT/metadata.txt" 2>&1; then
    ok "desktop file, metainfo and icons installed"
    cat "$OUT/metadata.txt"
else
    cat "$OUT/metadata.txt"
    bad "desktop integration"
fi

echo "RESULT pass=$pass fail=$fail skip=$skip"
[ "$fail" -eq 0 ]
