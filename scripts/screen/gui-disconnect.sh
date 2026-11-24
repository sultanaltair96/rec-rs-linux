#!/bin/sh
# Run inside headless.sh with REC_BIN and OUT_DIR. Real portal-node removal,
# retry recording, preview restart, then bounded control-quit/SIGTERM (10 each).
set -eu
python3 - <<'PY'
import atexit, json, os, pathlib, signal, socket, subprocess, time

binary = os.environ['REC_BIN']
out = pathlib.Path(os.environ['OUT_DIR']) / 'gui-disconnect'
out.mkdir(parents=True, exist_ok=True)
rt = pathlib.Path(os.environ['XDG_RUNTIME_DIR'])

def wait_for(check, label, app, seconds=25):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if check():
            return
        assert app.poll() is None, f'{label}: app exited {app.returncode}'
        time.sleep(.05)
    raise AssertionError(f'timeout: {label}')

def control(cmd):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(3)
        s.connect(str(rt / 'rec-rs-control.sock'))
        s.sendall((cmd + '\n').encode())
        assert s.recv(128) == b'ok\n', cmd

def registry():
    return json.loads(subprocess.check_output(['pw-dump']))

backends = []
@atexit.register
def stop_backends():
    for p in backends:
        if p.poll() is None:
            p.terminate()
    for p in backends:
        try:
            p.wait(timeout=3)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait()

def restart_backend(d):
    # xdpw treats external node destruction as a fatal core error and exits.
    # Restore the test desktop's backend before asking Rec to open a new
    # screen. D-Bus activation lacks this headless session's Wayland env.
    p = subprocess.Popen(['/usr/libexec/xdg-desktop-portal-wlr', '-r'],
                         stdout=(d / 'portal-restart.log').open('a'), stderr=subprocess.STDOUT)
    backends.append(p)
    time.sleep(.5)
    assert p.poll() is None, 'restarted portal backend exited'

for mode in ('control', 'sigterm'):
    for iteration in range(1, int(os.environ.get('GUI_DISCONNECT_REPEATS', '10')) + 1):
        d = out / f'{mode}-{iteration:02}'
        d.mkdir(exist_ok=True)
        if backends:
            restart_backend(d)
        settings = d / 'settings.json'
        settings.write_text(json.dumps(dict(output_dir=str(d), resolution='720p',
                                           fps=30, mic_enabled=False,
                                           system_audio=False, scene='screen')))
        logpath = d / 'app.log'
        def log():
            return logpath.read_text(errors='replace')
        env = dict(os.environ, REC_SETTINGS=str(settings), REC_DEBUG='1')
        with logpath.open('w') as f:
            app = subprocess.Popen([binary, 'gui'], stdout=f, stderr=subprocess.STDOUT, env=env)
        try:
            wait_for(lambda: 'first screen frame delivered' in log(), 'idle first frame', app, seconds=90)
            text = log()
            node = int(next(l.split()[-1] for l in text.splitlines()
                            if 'screen stream connected to node' in l))
            serial = next(l.split('"')[1] for l in text.splitlines()
                          if 'screen target registry.global:' in l)
            def target_present():
                return any(o['id'] == node and str(o.get('info', {}).get('props', {}).get('object.serial')) == serial
                           for o in registry())
            assert target_present(), 'portal ID/serial differs from registry'
            subprocess.run(['pw-cli', 'destroy', str(node)], check=True, capture_output=True)
            assert not target_present(), 'portal target not removed'
            wait_for(lambda: 'preview failed; cleaning up asynchronously' in log(), 'failed preview cleanup', app)
            restart_backend(d)
            # Send explicit Start twice: idempotent even while teardown owns
            # the old session. It must not be lost or toggle the new file off.
            control('start-recording')
            control('start-recording')
            wait_for(lambda: (rt / 'rec-rs-recorder.sock').exists(), 'retry recording socket', app)
            wait_for(lambda: log().count('first screen frame delivered') >= 2, 'retry first frame', app)
            time.sleep(2)
            control('stop')
            wait_for(lambda: not (rt / 'rec-rs-recorder.sock').exists(), 'retry finalized', app)
            files = list(d.glob('*.mov')) + list(d.glob('*.mp4'))
            assert len(files) == 1, files
            subprocess.run([binary, 'probe', str(files[0])], check=True,
                           stdout=(d / 'probe.log').open('w'), stderr=subprocess.STDOUT)
            subprocess.run(['ffmpeg', '-v', 'error', '-i', str(files[0]), '-f', 'null', '-'], check=True)
            # Recording completion restores the preview: prove a third real
            # stream delivered frames, not just that the control thread acks.
            wait_for(lambda: log().count('first screen frame delivered') >= 3, 'preview restarted', app)
            # Kill that idle preview too, then quit during its cleanup.
            text = log()
            node = int([l.split()[-1] for l in text.splitlines()
                        if 'screen stream connected to node' in l][-1])
            serial = [l.split('"')[1] for l in text.splitlines()
                      if 'screen target registry.global:' in l][-1]
            assert target_present()
            subprocess.run(['pw-cli', 'destroy', str(node)], check=True, capture_output=True)
            assert not target_present()
            wait_for(lambda: log().count('preview failed; cleaning up asynchronously') >= 2, 'second preview failure', app)
            start = time.monotonic()
            if mode == 'control':
                control('quit')
            else:
                app.send_signal(signal.SIGTERM)
            assert app.wait(timeout=2) == 0, log()
            elapsed = time.monotonic() - start
            assert 'event loop finished' in log() and 'app dropped' in log()
            assert 'panicked' not in log()
            assert not (rt / 'rec-rs.pid').exists()
            assert not (rt / 'rec-rs-recorder.sock').exists()
            print(f'PASS gui-disconnect {mode} {iteration}: 2 real disconnects, retry decoded, preview restarted, exit {elapsed:.3f}s', flush=True)
        finally:
            if app.poll() is None:
                app.kill()
                app.wait()
PY
