#!/bin/sh
# Opt-in fault matrix. Each fault gets a fresh private desktop; never target
# processes by name (the developer's desktop must remain untouched).
set -eu
if [ "${1:-}" != --inside ]; then
    mode=${1:?cli, gui or idle}
    case "$mode" in cli|gui|idle|dock) ;; *) exit 2;; esac
    failed=0
    faults='node-remove pipewire-stop portal-stop'
    if [ "$mode" = dock ]; then faults='dock-output-first dock-input-first'; mode=idle; fi
    for fault in $faults; do
        d="${OUT_DIR:?}/faults-$mode/$fault"
        mkdir -p "$d"
        rc=0
        OUT_DIR="$d" SCREEN_LOG_DIR="$d/harness" \
            timeout 180 scripts/screen/headless.sh sh scripts/faults/cases.sh --inside "$mode" "$fault" \
            >"$d/session.log" 2>&1 || rc=$?
        if [ "$rc" -eq 0 ]; then
            echo "PASS  faults-$mode-$fault (see session.log for verified assertions)"
        else
            echo "FAIL  faults-$mode-$fault (exit $rc; see $d/session.log)"
            failed=1
        fi
    done
    # There is no public PipeWire API to force these producer events on xdpw.
    # Do not confuse killing a node with exercising every stream failure.
    if [ "$faults" = 'node-remove pipewire-stop portal-stop' ]; then
        for fault in node-error eos format-change; do
            echo "SKIP  faults-$mode-$fault (no deterministic producer injection in the real portal harness)"
        done
    fi
    exit "$failed"
fi
shift
python3 - "$@" <<'PY'
import json, os, pathlib, signal, socket, subprocess, sys, time

mode, fault = sys.argv[1:]
assert mode in ('cli', 'gui', 'idle')
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

# Dock fixtures are owned children on headless.sh's private server. The app
# still consumes the REAL portal screen, not the unrelated synthetic producer.
dock = fault.startswith('dock-')
children = []
sink = None
def run(*args):
    return subprocess.check_output(args, timeout=10, stderr=subprocess.STDOUT).decode().strip()
def spawn(name, *args):
    f = (out / (name + '.log')).open('w')
    p = subprocess.Popen(args, stdout=f, stderr=subprocess.STDOUT)
    f.close()
    children.append(p)
    return p
def videos_add():
    global videos
    videos = [spawn(name, 'gst-launch-1.0', '-q', 'videotestsrc', 'is-live=true',
                    '!', caps, '!', 'pipewiresink', 'mode=provide',
                    'stream-properties=props,node.name=' + name + ',media.class=Video/Source')
              for name, caps in [('rec_fault_screen', 'video/x-raw,format=RGBx,width=1280,height=720,framerate=30/1'),
                                 ('rec_fault_camera', 'video/x-raw,format=YUY2,width=640,height=480,framerate=30/1')]]
    graph_wait(True)
def graph_wait(present):
    def check():
        nodes = json.loads(run('pw-dump'))
        for name in ('rec_fault_screen', 'rec_fault_camera'):
            found = [o for o in nodes if o.get('info', {}).get('props', {}).get('node.name') == name]
            if present:
                if len(found) != 1 or found[0]['info']['props'].get('media.class') != 'Video/Source': return False
            elif found: return False
        return True
    wait(check, 'synthetic video registry ' + str(present), 15)
def videos_remove():
    for p in videos:
        p.terminate()
        p.wait(timeout=5)
    graph_wait(False)
def sink_add():
    global sink
    sink = run('pactl', 'load-module', 'module-null-sink', 'sink_name=rec_fault_dock')
    run('pactl', 'set-default-sink', 'rec_fault_dock')
    assert run('pactl', 'get-default-sink') == 'rec_fault_dock'
    wait(lambda: any(o.get('info', {}).get('props', {}).get('node.name') == 'rec_fault_dock' and
                     o['info']['props'].get('media.class') == 'Audio/Sink'
                     for o in json.loads(run('pw-dump'))), 'dock sink registry')
def sink_remove():
    global sink
    run('pactl', 'unload-module', sink)
    sink = None
    assert run('pactl', 'get-default-sink') != 'rec_fault_dock'
    wait(lambda: not any(o.get('info', {}).get('props', {}).get('node.name') == 'rec_fault_dock'
                         for o in json.loads(run('pw-dump'))), 'dock sink removed')

monitor = None
if dock:
    monitor = spawn('portal-bus', 'dbus-monitor', '--session',
                    "type='method_call',interface='org.freedesktop.portal.ScreenCast'",
                    "type='signal',interface='org.freedesktop.portal.Request',member='Response'",
                    "type='method_call',interface='org.freedesktop.portal.Request',member='Close'")
    time.sleep(.3)

def portal_counts():
    import re
    text = (out / 'portal-bus.log').read_text(errors='replace')
    blocks = re.split(r'(?=^(?:method call|signal) time=)', text, flags=re.M)
    outstanding = set()
    maximum = requests = 0
    for block in blocks:
        header = block.splitlines()[0] if block else ''
        if 'interface=org.freedesktop.portal.ScreenCast;' in header:
            if not any('member=' + method in header for method in ('CreateSession', 'SelectSources', 'Start')):
                continue  # OpenPipeWireRemote returns an fd, not a Request.
            token = re.search(r'string "handle_token"\s+variant\s+string "([^"]+)"', block)
            sender = re.search(r'sender=:([\d.]+)', header)
            assert token and sender, 'unparsed portal call: ' + block
            path = '/org/freedesktop/portal/desktop/request/' + sender[1].replace('.', '_') + '/' + token[1]
            assert path not in outstanding
            outstanding.add(path)
            requests += 1
            maximum = max(maximum, len(outstanding))
        elif 'interface=org.freedesktop.portal.Request;' in header:
            path = re.search(r'path=([^; ]+)', header)
            if path: outstanding.discard(path[1])
    assert requests >= 6, (requests, text)
    assert maximum == 1 and not outstanding, (maximum, outstanding, text)
    print(f'verified portal counter: requests={requests} max_outstanding={maximum} final={len(outstanding)}', flush=True)

env = dict(os.environ, REC_DEBUG='1')
if mode in ('gui', 'idle'):
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
    if mode in ('gui', 'idle'):
        wait(lambda: (rt / 'rec-rs-control.sock').exists() and
             'first screen frame delivered' in log(), 'GUI preview and control socket', 90)
        frame_marker = log().count('first screen frame delivered')
        if mode == 'gui':
            control('start-recording')
    if mode != 'idle':
        wait(lambda: log().count('first screen frame delivered') > frame_marker and
             (rt / 'rec-rs-recorder.sock').exists(), 'recording first frame', 90)
    time.sleep(2)
    if dock:
        spawn('pulse', 'pipewire-pulse')
        wait(lambda: (rt / 'pulse/native').exists(), 'private Pulse socket')
        sink_add()
        videos_add()
    text = log()
    node = int([l.split()[-1] for l in text.splitlines() if 'screen stream connected to node' in l][-1])
    serial = [l.split('"')[1] for l in text.splitlines() if 'screen target registry.global:' in l][-1]
    def registry():
        return json.loads(subprocess.check_output(['pw-dump'], timeout=5))
    def present():
        return any(o['id'] == node and str(o.get('info', {}).get('props', {}).get('object.serial')) == serial
                   for o in registry())
    assert present(), 'capture node identity mismatch'
    # Only export the runtime/display after startup: exporting the display
    # before startup lets the container's locked keyring show a blocking dialog.
    # xdpw can exit on producer destruction and needs these on reactivation.
    subprocess.run(['dbus-update-activation-environment', 'XDG_RUNTIME_DIR',
                    'WAYLAND_DISPLAY', 'XDG_CURRENT_DESKTOP', 'XDG_SESSION_TYPE',
                    'XDG_CONFIG_HOME', 'XDG_STATE_HOME', 'XDG_CACHE_HOME'], check=True, timeout=5)
    if dock:
        # Removal burst includes the actual granted screen node. Synthetic
        # camera is advertised only: native app camera capture uses V4L2.
        if fault == 'dock-output-first': sink_remove()
        videos_remove()
        subprocess.run(['pw-cli', 'destroy', str(node)], check=True, capture_output=True, timeout=5)
        assert not present(), 'portal node destruction rejected'
        if fault == 'dock-input-first': sink_remove()
        if fault == 'dock-output-first': sink_add()
        videos_add()
        if fault == 'dock-input-first': sink_add()
        print('verified combined dock graph removal/return: ' + fault, flush=True)
    elif fault == 'node-remove':
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
    if mode == 'idle':
        # Only node-remove has a live portal service available for reacquisition.
        # Service-stop cases deliberately cannot recover without a restart.
        if fault == 'node-remove' or dock:
            wait(lambda: log().count('first screen frame delivered') > frame_marker,
                 'idle preview reacquired a frame after source removal', 60)
        assert not (rt / 'rec-rs-recorder.sock').exists(), 'idle started a recorder'
        assert not list(out.glob('*.mp4')), 'idle created a recording'
        control('quit')
        assert app.wait(timeout=5) == 0, log()
        assert 'panicked' not in log(), log()
        if dock:
            time.sleep(.2)
            portal_counts()
        print(f'verified idle {fault}: bounded shutdown' +
              (' and preview recovery' if fault == 'node-remove' or dock else '; recovery not claimed'), flush=True)
        sys.exit(0)
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
    if sink is not None:
        try: sink_remove()
        except Exception: pass
    for p in reversed(children):
        if p.poll() is None:
            p.terminate()
            try: p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()
    if app.poll() is None:
        app.kill()
        app.wait()
PY
