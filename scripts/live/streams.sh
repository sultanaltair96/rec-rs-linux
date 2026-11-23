#!/bin/sh
# Streaming checks against a local MediaMTX: one shared encode to several
# destinations at once, an RTMPS connection with a self-signed certificate, a
# WHIP endpoint, enhanced-RTMP HEVC (and the H.264 fallback), and a reconnect
# when the server restarts.
#
#   scripts/live/streams.sh              # 2 RTMP + 1 RTMPS + 1 WHIP, HEVC, fallback
#   scripts/live/streams.sh --rtmp       # the multi-destination + sync checks only
#   scripts/live/streams.sh --whip       # one WHIP endpoint and its sync
#   scripts/live/streams.sh --reconnect  # restart MediaMTX mid-stream
#   scripts/live/streams.sh --hevc       # HEVC over enhanced RTMP and the fallback
#
# Every case prints PASS/FAIL lines and the script exits non-zero on a failure.
# Output goes to ${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}/live/streams.
set -eu
cd "$(dirname "$0")/../.."
. scripts/live/lib.sh

REC=$(rec_binary)
OUT="$(live_root)/streams"
TIMEOUT="${REC_TIMEOUT:-180}"
# The self-signed certificate is the server's own: this is the one place where
# certificate verification is deliberately off (RTMPS only).
export REC_INSECURE_TLS=1
export REC_STREAM_KEY="${REC_STREAM_KEY:-selftest-key}"

pass=0
fail=0
ok() {
    echo "PASS  $1  ($2)"
    pass=$((pass + 1))
}
bad() {
    echo "FAIL  $1  ($2)"
    fail=$((fail + 1))
}
# check_and_count NAME FILE MAX_MS: check a recording and count it.
check_and_count() {
    line=$(check_recording "$1" "$2" "$3" || true)
    echo "$line"
    case $line in
        PASS*) pass=$((pass + 1)) ;;
        *) fail=$((fail + 1)) ;;
    esac
}

# run_harness NAME SECS ARGS...: run the live harness, keep its output, and
# print it when it fails.
# run_harness NAME SECS FLAGS URL...: FLAGS is a word-separated list for the
# harness (e.g. "--codec hevc"), each URL becomes one --stream.
run_harness() {
    name=$1
    secs=$2
    flags=$3
    shift 3
    streams=""
    for url in "$@"; do
        streams="$streams --stream $url"
    done
    # shellcheck disable=SC2086
    if timeout "$TIMEOUT" "$REC" live-selftest --secs "$secs" --resolution 720p --fps 30 \
        --output-dir "$OUT" $flags $streams >"$OUT/$name.out" 2>"$OUT/$name.err"; then
        return 0
    fi
    echo "live: harness failed ($name), stderr:" >&2
    tr '\r' '\n' <"$OUT/$name.err" | grep -v '^\[2K' | tail -15 >&2
    return 1
}

# --- multi-destination ------------------------------------------------------
case_multi() {
    dir="$OUT/multi"
    rm -rf "$dir"
    mkdir -p "$dir"
    start_server "$dir" "$dir/mediamtx.log"
    if run_harness multi 16 "" \
        rtmp://127.0.0.1:1935/a \
        rtmp://127.0.0.1:1935/b \
        rtmps://127.0.0.1:1936/c \
        http://127.0.0.1:8889/d/whip; then
        live=$(grep -c 'live: stream .* live ' "$OUT/multi.out" || true)
        reconnects=$(grep -o 'reconnects [0-9]*' "$OUT/multi.out" | awk '{s += $2} END {print s + 0}')
        if [ "$live" -eq 4 ] && [ "$reconnects" -eq 0 ]; then
            ok stream-multi "4/4 destinations live at once, no reconnects"
        else
            bad stream-multi "$live/4 live, $reconnects reconnects"
        fi
    else
        bad stream-multi "the harness failed"
    fi
    stop_server "$dir"
    check_and_count rtmp-a "$(newest_recording "$dir/recordings/a")" 40
    check_and_count rtmp-b "$(newest_recording "$dir/recordings/b")" 40
    check_and_count rtmps "$(newest_recording "$dir/recordings/c")" 40
    check_and_count whip "$(newest_recording "$dir/recordings/d")" 40
}

# --- WHIP: five consecutive sessions, each within ±15 ms --------------------
# The start of a WHIP session is where a server-side A/V offset comes from (each
# track's first packet sets its timeline), so one run says little: five runs in a
# row must all land in the same place.
case_whip() {
    dir="$OUT/whip"
    rm -rf "$dir"
    mkdir -p "$dir"
    start_server "$dir" "$dir/mediamtx.log"
    worst=0
    runs=0
    for run in 1 2 3 4 5; do
        attempt=1
        while :; do
            path="w$run"
            [ "$attempt" -gt 1 ] && path="w$run-r$attempt"
            if run_harness "whip-$path" 8 "" "http://127.0.0.1:8889/$path/whip"; then
                if grep -q 'live: stream whip://.* live .* ever-live yes' "$OUT/whip-$path.out"; then
                    [ "$attempt" -eq 1 ] && runs=$((runs + 1))
                else
                    bad "whip-run-$run" "the WHIP destination never went live"
                fi
            else
                bad "whip-run-$run" "the harness failed"
            fi
            file=$(newest_recording "$dir/recordings/$path")
            if [ -z "$file" ]; then
                bad "whip-run-$run" "the server recorded nothing"
                break
            fi
            # The bar is the mean of the run's pairs (±15 ms, as upstream
            # measures). synccheck's own bound is looser so a single outlier
            # frame is reported rather than ending the check.
            line=$(python3 scripts/synccheck.py "$file" --max-ms 45 2>&1 | grep 'audio 0' | head -1)
            mean=$(echo "$line" | sed 's/.*offset mean //; s/ ms.*//')
            worst=$(echo "$line" | sed 's/.*worst //; s/ ms.*//')
            if [ -n "$mean" ] && awk -v m="$mean" 'BEGIN { exit !(m >= -15 && m <= 15) }'; then
                ok "whip-run-$run" "A/V offset mean $mean (worst $worst)"
                break
            fi
            if [ "$attempt" -ge 2 ]; then
                bad "whip-run-$run" "A/V offset mean ${mean:-unmeasurable}, worst ${worst:-?}"
                break
            fi
            # One retry: under load MediaMTX's WebRTC receiver occasionally
            # mangles a session's first second (no beeps pair with a flash at
            # all). Our sender logged no drops; a second session tells whether
            # the code or the box was at fault.
            echo "live: whip run $run: $line; retrying once"
            attempt=$((attempt + 1))
        done
    done
    if [ "$runs" -eq 5 ]; then
        ok whip-sessions "5/5 WHIP sessions published"
    else
        bad whip-sessions "$runs/5 WHIP sessions published"
    fi
    codec=$(ffprobe -v error -select_streams a -show_entries stream=codec_name -of csv=p=0 \
        "$(newest_recording "$dir/recordings/w5")" 2>/dev/null || true)
    if [ "$codec" = opus ]; then
        ok whip-audio "audio is Opus, as WebRTC requires"
    else
        bad whip-audio "audio codec is '${codec:-none}'"
    fi
    stop_server "$dir"
}

# --- HEVC over enhanced RTMP, then the H.264 fallback ----------------------
case_hevc() {
    dir="$OUT/hevc"
    rm -rf "$dir"
    mkdir -p "$dir"
    start_server "$dir" "$dir/mediamtx.log"
    if run_harness hevc 8 "--codec hevc" rtmp://127.0.0.1:1935/hevc; then
        file=$(newest_recording "$dir/recordings/hevc")
        if [ -n "$file" ] && ffprobe -v error -select_streams v -show_entries stream=codec_name -of csv=p=0 "$file" | grep -qx hevc; then
            ok stream-hevc "enhanced RTMP carried HEVC (hvc1), $(basename "$file")"
            check_and_count hevc-sync "$file" 40
        else
            bad stream-hevc "the server's recording is not HEVC"
        fi
        if grep -q 'HEVC Main (an 8-bit encoder' "$OUT/hevc.err"; then
            ok hevc-main10-note "logged that this image can only do HEVC Main"
        else
            bad hevc-main10-note "no note about Main10"
        fi
    else
        bad stream-hevc "the harness failed"
    fi
    # A server that answers the enhanced-RTMP connect without hvc1: the encode
    # has to switch to H.264 and keep streaming (REC_LIVE_HEVC_REFUSE simulates
    # it; MediaMTX itself accepts HEVC).
    if REC_LIVE_HEVC_REFUSE=1 run_harness fallback 8 "--codec hevc" rtmp://127.0.0.1:1935/fallback; then
        file=$(newest_recording "$dir/recordings/fallback")
        codec=$(ffprobe -v error -select_streams v -show_entries stream=codec_name -of csv=p=0 "$file" 2>/dev/null || true)
        if [ "$codec" = h264 ] && grep -q 'does not take HEVC; using H.264\|does not take HEVC; switching to H.264' "$OUT/fallback.err"; then
            ok stream-hevc-fallback "the encode fell back to H.264 with a note"
        else
            bad stream-hevc-fallback "codec '${codec:-none}'"
        fi
    else
        bad stream-hevc-fallback "the harness failed"
    fi
    stop_server "$dir"
}

# --- REC_INSECURE_TLS must not weaken a real stream ------------------------
# The flag may switch certificate verification off for a loopback test server
# only. Reach the same self-signed MediaMTX by a non-loopback address and it must
# be ignored (with a warning), so the connection is refused; on localhost the
# same flag lets the self-signed certificate through.
case_insecure_policy() {
    dir="$OUT/insecure"
    rm -rf "$dir"
    mkdir -p "$dir"
    REC_MEDIAMTX_BIND=0.0.0.0 start_server "$dir" "$dir/mediamtx.log"
    ip=$(python3 - <<'EOF'
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
try:
    s.connect(("10.255.255.255", 1))
    print(s.getsockname()[0])
except OSError:
    print("")
EOF
)
    if [ -z "$ip" ] || [ "${ip#127.}" != "$ip" ]; then
        bad insecure-policy "no non-loopback address to test with (${ip:-none})"
        stop_server "$dir"
        return 0
    fi
    # Non-loopback: the flag is ignored, verification happens, the self-signed
    # certificate is refused, and the warning names the host.
    REC_INSECURE_TLS=1 timeout "$TIMEOUT" "$REC" live-selftest --secs 6 --resolution 720p --fps 30 \
        --output-dir "$OUT" --stream "rtmps://$ip:1936/policy" \
        >"$OUT/insecure.out" 2>"$OUT/insecure.err" && status=0 || status=$?
    if [ "$status" -ne 0 ] \
        && grep -q "REC_INSECURE_TLS is set but $ip is not a loopback host" "$OUT/insecure.err" \
        && grep -qi "certificate" "$OUT/insecure.err"; then
        ok insecure-policy "ignored for $ip (certificate refused, warned)"
    else
        bad insecure-policy "status $status: $(tr '\r' '\n' <"$OUT/insecure.err" | grep -iE 'certificate|REC_INSECURE' | tail -1)"
    fi
    # Loopback: the same flag lets the self-signed test server through.
    if run_harness insecure-loopback 6 "" rtmps://localhost:1936/loopback \
        && grep -q 'live: stream rtmps://localhost live' "$OUT/insecure-loopback.out"; then
        ok insecure-loopback "accepted on localhost, as the self-test needs"
    else
        bad insecure-loopback "the loopback stream did not go live"
    fi
    stop_server "$dir"
}

# --- a server restart mid-stream -------------------------------------------
case_reconnect() {
    dir="$OUT/reconnect"
    rm -rf "$dir"
    mkdir -p "$dir"
    start_server "$dir" "$dir/mediamtx.log"
    (
        timeout "$TIMEOUT" "$REC" live-selftest --secs 24 --resolution 720p --fps 30 \
            --output-dir "$OUT" --stream rtmp://127.0.0.1:1935/r \
            >"$OUT/reconnect.out" 2>"$OUT/reconnect.err"
    ) &
    harness=$!
    sleep 7
    stop_server "$dir"
    sleep 4
    start_server "$dir" "$dir/mediamtx.log.2"
    if wait "$harness"; then
        reconnects=$(grep -o 'reconnects [0-9]*' "$OUT/reconnect.out" | awk '{s += $2} END {print s + 0}')
        if [ "${reconnects:-0}" -ge 1 ] && grep -q 'live: stream rtmp://.* live .* ever-live yes' "$OUT/reconnect.out"; then
            ok stream-reconnect "reconnected after the restart ($reconnects reconnects, live at the end)"
        else
            bad stream-reconnect "$reconnects reconnects, not live at the end"
        fi
    else
        bad stream-reconnect "the harness failed"
        tr '\r' '\n' <"$OUT/reconnect.err" | grep -v '^\[2K' | tail -10 >&2
    fi
    stop_server "$dir"
}

mode=${1:---all}
# MediaMTX is downloaded at test time into the cache (outside the repo); when
# that is not possible the caller skips instead of failing.
scripts/live/mediamtx.sh ensure >/dev/null 2>&1 || exit 3
# Each case clears only its own folder, so a failing run leaves its recordings
# and logs behind for inspection.
mkdir -p "$OUT"
case "$mode" in
    --all) case_multi && case_whip && case_hevc && case_reconnect && case_insecure_policy ;;
    --insecure) case_insecure_policy ;;
    --rtmp) case_multi && case_reconnect ;;
    --whip) case_whip ;;
    --hevc) case_hevc ;;
    --reconnect) case_reconnect ;;
    *)
        echo "usage: $0 [--rtmp|--whip|--hevc|--reconnect|--insecure]" >&2
        exit 2
        ;;
esac

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
