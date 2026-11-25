#!/bin/sh
# Opt-in fault matrix. Each fault gets a fresh private desktop; never target
# processes by name (the developer's desktop must remain untouched).
set -eu
if [ "${1:-}" != --inside ]; then
    mode=${1:?cli or gui}
    failed=0
    for fault in node-remove pipewire-stop portal-stop; do
        d="${OUT_DIR:?}/faults-$mode/$fault"
        mkdir -p "$d"
        rc=0
        OUT_DIR="$d" SCREEN_LOG_DIR="$d/harness" \
            timeout 180 scripts/screen/headless.sh sh scripts/faults/cases.sh --inside "$mode" "$fault" \
            >"$d/session.log" 2>&1 || rc=$?
        if [ "$rc" -eq 0 ]; then
            echo "PASS  faults-$mode-$fault (verified injection, bounded teardown, decoded file)"
        else
            echo "FAIL  faults-$mode-$fault (exit $rc; see $d/session.log)"
            failed=1
        fi
    done
    # There is no public PipeWire API to force these producer events on xdpw.
    # Do not confuse killing a node with exercising every stream failure.
    for fault in node-error eos format-change; do
        echo "SKIP  faults-$mode-$fault (no deterministic producer injection in the real portal harness)"
    done
    exit "$failed"
fi
shift
python3 - "$@" <<'PY'
import json, os, pathlib, signal, socket, subprocess, sys, time

mode, fault = sys.argv[1:]
assert mode in ('cli', 'gui')
binary = os.environ['REC_BIN']
out = pathlib.Path(os.environ['OUT_DIR'])
rt = pathlib.Path(os.environ['XDG_RUNTIME_DIR'])
assert os.environ.get('REC_HEADLESS_SESSION') == '1', 'private harness required'
logpath = out / 'app.log'
def log():
    return logpath.read_text(errors='replace')
def wait(check, label, seconds=35):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if check():
            return
        assert app.poll() is None, f'{label}: premature exit {app.returncode}: {log()}'
        time.sleep(.1)
    raise AssertionError('timeout: ' + label)
def control(cmd):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(3)
        s.connect(str(rt / 'rec-rs-control.sock'))
        s.sendall((cmd + '\n').encode())
        assert s.recv(128) == b'ok\n', cmd

env = dict(os.environ, REC_DEBUG='1')
if mode == 'gui':
    settings = out / 'settings.json'
    settings.write_text(json.dumps(dict(output_dir=str(out), resolution='720p', fps=30,
                                       mic_enabled=False, system_audio=False, scene='screen')))
    env['REC_SETTINGS'] = str(settings)
    args = ['gui']
else:
    args = ['record', '-t', '120', '--no-mic', '--no-system-audio', '-o', str(out / 'clip.mp4')]
with logpath.open('w') as f:
    app = subprocess.Popen([binary] + args, env=env, stdout=f, stderr=subprocess.STDOUT)
try:
    frame_marker = 0
    if mode == 'gui':
        wait(lambda: (rt / 'rec-rs-control.sock').exists() and
             'first screen frame delivered' in log(), 'GUI preview and control socket', 90)
        frame_marker = log().count('first screen frame delivered')
        control('start-recording')
    wait(lambda: log().count('first screen frame delivered') > frame_marker and
         (rt / 'rec-rs-recorder.sock').exists(), 'recording first frame', 90)
    time.sleep(2)
    text = log()
    node = int([l.split()[-1] for l in text.splitlines() if 'screen stream connected to node' in l][-1])
    serial = [l.split('"')[1] for l in text.splitlines() if 'screen target registry.global:' in l][-1]
    def registry():
        return json.loads(subprocess.check_output(['pw-dump'], timeout=5))
    def present():
        return any(o['id'] == node and str(o.get('info', {}).get('props', {}).get('object.serial')) == serial
                   for o in registry())
    assert present(), 'capture node identity mismatch'
    if fault == 'node-remove':
        subprocess.run(['pw-cli', 'destroy', str(node)], check=True, capture_output=True, timeout=5)
        assert not present(), 'node destruction rejected'
    else:
        # Resolve exact service PID via this private session's socket / bus,
        # then verify its UID and environment before signalling anything.
        if fault == 'portal-stop':
            reply = subprocess.check_output(['gdbus', 'call', '--session', '--dest', 'org.freedesktop.DBus',
                '--object-path', '/org/freedesktop/DBus', '--method',
                'org.freedesktop.DBus.GetConnectionUnixProcessID', 'org.freedesktop.impl.portal.desktop.wlr'], timeout=5)
            import re
            pid = int(re.search(rb'uint32 (\d+)', reply)[1])
        else:
            candidates = []
            for p in pathlib.Path('/proc').glob('[0-9]*'):
                try:
                    if (p / 'comm').read_text().strip() == 'pipewire' and \
                       ('XDG_RUNTIME_DIR=' + str(rt)).encode() in (p / 'environ').read_bytes().split(b'\0'):
                        candidates.append(int(p.name))
                except (OSError, PermissionError):
                    pass
            assert len(candidates) == 1, candidates
            pid = candidates[0]
        proc = pathlib.Path('/proc') / str(pid)
        assert proc.stat().st_uid == os.getuid()
        assert ('XDG_RUNTIME_DIR=' + str(rt)).encode() in (proc / 'environ').read_bytes().split(b'\0')
        os.kill(pid, signal.SIGKILL)
        # SIGKILL is not merely a requested graceful shutdown.
        print(f'injected {fault}: private service pid {pid}', flush=True)
    start = time.monotonic()
    if mode == 'cli':
        assert app.wait(timeout=30) == 0, log()
    else:
        wait(lambda: not (rt / 'rec-rs-recorder.sock').exists(), 'fault recording finalized', 30)
        control('quit')
        assert app.wait(timeout=2) == 0, log()
    assert time.monotonic() - start < 40
    assert 'panicked' not in log(), log()
    assert not (rt / 'rec-rs-recorder.sock').exists(), 'stale recorder socket'
    files = list(out.glob('*.mp4')) + list(out.glob('*.mov'))
    assert len(files) == 1, files
    with (out / 'probe.log').open('w') as f:
        subprocess.run([binary, 'probe', str(files[0])], check=True, stdout=f, stderr=subprocess.STDOUT, timeout=30)
    frames = subprocess.check_output(['ffprobe', '-v', 'error', '-select_streams', 'v:0', '-count_frames',
        '-show_entries', 'stream=nb_read_frames', '-of', 'csv=p=0', str(files[0])], timeout=30)
    assert int(frames.strip()) >= 15, frames
    subprocess.run(['ffmpeg', '-v', 'error', '-xerror', '-i', str(files[0]), '-f', 'null', '-'], check=True, timeout=30)
    print(f'verified {mode} {fault}: {frames.strip().decode()} decoded frames', flush=True)
finally:
    if app.poll() is None:
        app.kill()
        app.wait()
PY
