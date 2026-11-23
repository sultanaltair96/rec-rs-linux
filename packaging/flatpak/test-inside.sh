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
# The writer prints which encoder it chose ("rec: debug: encoder x264enc") only
# with REC_DEBUG, so every record run below runs with it on.
export REC_DEBUG=1

ok() { echo "PASS  $1"; pass=$((pass + 1)); }
bad() { echo "FAIL  $1"; fail=$((fail + 1)); }
skipping() { echo "SKIP  $1 ($2)"; skip=$((skip + 1)); }

echo "== the installed app"
flatpak list | grep "$APP" | tee "$OUT/installed.txt"

echo "== encoders and capture elements inside the sandbox"
{
    for el in x264enc x265enc openh264enc avenc_aac fdkaacenc fdkaacdec avdec_aac \
              aacparse opusenc opusdec avenc_prores_aw avenc_prores avenc_prores_ks \
              pipewiresrc appsrc appsink \
              videoconvert videoscale mp4mux qtmux qtdemux h264parse h265parse \
              avdec_h264 avdec_h265; do
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
    grep -o 'encoder [a-z0-9_]*: chosen' "$OUT/h264.log" | head -1
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
    grep -o 'encoder [a-z0-9_]*: chosen' "$OUT/hevc.log" | head -1
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
    grep -o 'encoder [a-z0-9_]*: chosen' "$OUT/prores.log" | head -1
    ok "record prores ($(grep -o 'encoder [a-z0-9_]*: chosen' "$OUT/prores.log" | head -1 | sed 's/: chosen//'))"
else
    cat "$OUT/prores.log"
    bad "record prores"
fi

echo "== GUI: no subcommand opens the window (Xvfb, software rendering)"
# The container has no desktop, so an Xvfb display stands in for one and the
# runtime's Mesa renders with llvmpipe. A window that is still up after 8 s is
# the proof that the app started; the fake display then gets a SIGTERM, which
# the app treats as quit (and would finalize a recording).
XVFB_RUN=$(command -v xvfb-run || echo /usr/sbin/xvfb-run)
if [ -x "$XVFB_RUN" ]; then
    start=$(date +%s)
    rc=0
    "$XVFB_RUN" -a timeout -s TERM 8 flatpak run "$APP" >"$OUT/gui.log" 2>&1 || rc=$?
    elapsed=$(( $(date +%s) - start ))
    if [ "$rc" -eq 124 ] && [ "$elapsed" -ge 7 ] && ! grep -q 'panicked' "$OUT/gui.log"; then
        ok "the app runs with no subcommand and keeps its window for 8 s"
    else
        echo "  exit $rc after ${elapsed}s"
        tail -20 "$OUT/gui.log"
        bad "the app starts with no subcommand"
    fi
    head -5 "$OUT/gui.log" | sed 's/^/  /'
else
    skipping "the app starts with no subcommand" "no xvfb-run in the test container"
fi

echo "== live: RTMP + WHIP + replay from inside the sandbox"
if [ ! -f /tmp/mediamtx/.started ]; then
    skipping "streaming from inside the sandbox" "MediaMTX did not start in this container"
else
# MediaMTX runs in the test container (started by test-flatpak.sh); the sandbox
# shares the network namespace, so 127.0.0.1 is the same machine. The server's
# recordings are copied into ~/Videos afterwards so the sandbox can probe them.
export REC_STREAM_KEY=flatpak-test-key
live_log="$OUT/live.log"
rc=0
flatpak run --command=rec-rs "$APP" record --test-pattern --no-mic \
    -r 720p --fps 30 -t 8 \
    --stream rtmp://127.0.0.1:1935/rtmppkg \
    --stream http://127.0.0.1:8889/whippkg/whip \
    --replay 15 -o "$HOME/Videos/live.mp4" >"$live_log" 2>&1 &
rec=$!
sleep 5
# A second `flatpak run` gets its own PID namespace, so the pid the pidfile
# holds (the sandbox's own numbering) names a different process there: the
# signal hits the caller itself and it dies with 128+SIGRTMIN+1 = 163. The
# runtime directory is shared (the pidfile is visible), so the fix is the unix
# control socket the port plan already names for save-replay; until then this
# is a documented limitation, not a silent failure.
save_rc=0
flatpak run --command=rec-rs "$APP" save-replay >>"$OUT/live-save.log" 2>&1 || save_rc=$?
if [ "$save_rc" -eq 0 ]; then
    ok "save-replay reaches the running recorder from a second flatpak run (control socket)"
else
    cat "$OUT/live-save.log"
    bad "save-replay from a second flatpak run (exit $save_rc)"
fi
wait "$rec" || rc=$?
status=$(tr '\r' '\n' <"$live_log")
if [ "$rc" -eq 0 ]; then ok "record --stream --replay finished cleanly"; else echo "$status" | tail -8; bad "record --stream --replay"; fi
if echo "$status" | grep -q 'rtmp://127.0.0.1 live' && echo "$status" | grep -q 'whip://127.0.0.1 live'; then
    ok "both destinations were live while recording"
else
    bad "a destination never went live"
fi
# The replay buffer itself: the status line shows it filling up.
if echo "$status" | grep -qE 'replay [1-9][0-9]*s [0-9.]+ MB'; then
    ok "the replay buffer filled while streaming ($(echo "$status" | grep -oE 'replay [1-9][0-9]*s [0-9.]+ MB' | tail -1))"
else
    bad "the replay buffer never filled"
fi
replay=$(echo "$status" | grep -o 'replay saved .*' | head -1 | sed 's/replay saved //')
if [ -n "$replay" ] && flatpak run --command=rec-rs "$APP" probe "$replay" >"$OUT/live-replay.probe" 2>&1; then
    grep '^track' "$OUT/live-replay.probe" | sed 's/^/  /'
    ok "the replay clip plays ($(basename "$replay"))"
else
    cat "$OUT/live-replay.probe" 2>/dev/null
    bad "the replay clip"
fi

# The server's own recordings: one from RTMP, one from WHIP. fMP4 segments are
# finalised when the connection closes, so give the server a moment first.
sleep 3
mkdir -p "$HOME/Videos/mediamtx"
cp -r /tmp/mediamtx/recordings/. "$HOME/Videos/mediamtx/" 2>/dev/null || true
find "$HOME/Videos/mediamtx" -name '*.mp4' >"$OUT/live-recordings.txt" 2>/dev/null || true
sed 's/^/  /' "$OUT/live-recordings.txt"
for kind in rtmp whip; do
    case "$kind" in
        rtmp) dir=$(find "$HOME/Videos/mediamtx" -maxdepth 1 -type d -name rtmppkg | head -1) ;;
        whip) dir=$(find "$HOME/Videos/mediamtx" -maxdepth 1 -type d -name whippkg | head -1) ;;
    esac
    # MediaMTX puts an RTMP stream key in a subdirectory of the path.
    newest=$(find "${dir:-/nonexistent}" -name '*.mp4' 2>/dev/null | head -1)
    if [ -n "$newest" ] && flatpak run --command=rec-rs "$APP" probe "$newest" >"$OUT/live-$kind.probe" 2>&1; then
        tracks=$(grep '^track' "$OUT/live-$kind.probe" | tr '\n' ' ')
        audio=$(printf '%s' "$tracks" | grep -oE '(soun|audio) [A-Za-z0-9]+' | head -1)
        video=$(printf '%s' "$tracks" | grep -oE '(vide|video) [A-Za-z0-9]+' | head -1)
        if [ -n "$video" ] && [ -n "$audio" ]; then
            ok "MediaMTX recorded $kind with $video and $audio"
        else
            echo "  $tracks"
            bad "MediaMTX $kind recording has video and audio"
        fi
    else
        cat "$OUT/live-$kind.probe" 2>/dev/null
        bad "MediaMTX has no $kind recording"
    fi
done

fi

echo "== hardware encoding: the writer picks the VA encoder"
if [ -e /dev/dri/renderD128 ]; then
    if flatpak run --env=REC_DEBUG=1 --command=rec-rs "$APP" record --test-pattern \
            -r 1080p -t 3 --no-mic --codec h264 -o "$HOME/Videos/va.mp4" \
            >"$OUT/va-encode.log" 2>&1 &&
       grep -q 'encoder vah264enc: chosen' "$OUT/va-encode.log"; then
        ok "the writer picks vah264enc on this GPU ($(grep -o 'encoder vah264enc: chosen ([A-Z0-9_]*)' "$OUT/va-encode.log" | head -1))"
    else
        tr '\r' '\n' <"$OUT/va-encode.log" | tail -10
        bad "the writer picks vah264enc on this GPU"
    fi
else
    skipping "the writer picks vah264enc on this GPU" "no /dev/dri/renderD128 in this container"
fi

echo "== second-instance control: the socket, not the pid"
# Phase 1E gives the recorder a control socket next to the app's own, so
# stop/pause/resume/save-replay work from another sandbox even though the
# pidfile's pid is local to the recorder's PID namespace. This is the 1E
# scenario: an idle app, a CLI recording, a pidfile whose pid is bogus, and a
# stop that must still land.
rt="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
Xvfb :99 -screen 0 1280x800x24 >"$OUT/xvfb-ctl.log" 2>&1 &
xvfb_pid=$!
sleep 2
DISPLAY=:99 flatpak run "$APP" >"$OUT/app-idle.log" 2>&1 &
app_pid=$!
sleep 3
flatpak run --command=rec-rs "$APP" record --test-pattern --no-mic --no-system-audio \
    -t 30 -o /tmp/cli.mp4 >"$OUT/cli-record.log" 2>&1 &
cli_pid=$!
sleep 4

if flatpak run --command=rec-rs "$APP" record --test-pattern --no-mic --no-system-audio \
        -t 1 -o /tmp/second.mp4 >"$OUT/cli-second.log" 2>&1; then
    bad "a second recorder was allowed to start"
else
    if grep -q 'another recorder is running' "$OUT/cli-second.log"; then
        ok "a second recorder is refused by the control socket"
    else
        cat "$OUT/cli-second.log"
        bad "a second recorder's refusal"
    fi
fi

printf '999999\n/tmp/cli.mp4\n' >"$rt/rec-rs.pid"
stop_rc=0
stop_out=$(flatpak run --command=rec-rs "$APP" stop 2>&1) || stop_rc=$?
if [ "$stop_rc" -eq 0 ]; then
    case "$stop_out" in
        *"/tmp/cli.mp4"*) ok "stop crossed the sandbox and printed /tmp/cli.mp4" ;;
        *) echo "  $stop_out"; bad "stop printed the wrong path" ;;
    esac
else
    echo "  $stop_out"
    bad "stop from a second instance (exit $stop_rc)"
fi
wait "$cli_pid" 2>/dev/null || true

if kill -0 "$app_pid" 2>/dev/null; then
    ok "the idle app is still running after the CLI recording"
else
    tail -5 "$OUT/app-idle.log"
    bad "the idle app is still running"
fi
kill "$app_pid" "$xvfb_pid" 2>/dev/null || true
wait "$app_pid" 2>/dev/null || true
wait "$xvfb_pid" 2>/dev/null || true

echo "== Secret Service: stream-key through the sandbox"
# A keyring needs a session bus and an unlocked login keyring; test-flatpak.sh
# starts dbus-run-session + gnome-keyring for this shell when it is available.
if [ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    key_rc=0
    printf '%s\n' test-key |
        flatpak run --command=rec-rs "$APP" stream-key rtmp://127.0.0.1:1935/live >"$OUT/keyring.log" 2>&1 || key_rc=$?
    flatpak run --command=rec-rs "$APP" stream-key rtmp://127.0.0.1:1935/live --delete \
        >>"$OUT/keyring.log" 2>&1 || key_rc=$?
    case "$key_rc" in
        0) ok "stream-key stored and deleted through org.freedesktop.secrets" ;;
        *) cat "$OUT/keyring.log"
           if grep -q 'Secret Service is not available' "$OUT/keyring.log"; then
               skipping "stream-key through the Secret Service" "the keyring daemon did not come up"
           else
               bad "stream-key through the Secret Service"
           fi ;;
    esac
else
    skipping "stream-key through the Secret Service" "no session bus in the test container"
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
