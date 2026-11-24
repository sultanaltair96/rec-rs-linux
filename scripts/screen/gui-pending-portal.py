#!/usr/bin/env python3
"""Exercise the real GUI against a never-answering ScreenCast Start request.

Window visibility, rendered screenshot and live SNI properties are checked by
2 seconds. Screenshot is retained for wait-text inspection; the separate UI
unit test proves the starting state's rendered text. No OCR dependency.
"""
import json
import os
import pathlib
import socket
import subprocess
import tempfile
import time

out = pathlib.Path(os.environ.get('REC_TEST_DIR', str(pathlib.Path.home() / 'Videos/rec-rs-tests'))) / 'gui-pending-portal'
out.mkdir(parents=True, exist_ok=True)
duration = float(os.environ.get('PENDING_PORTAL_SECONDS', '185'))
assert duration >= 180, 'must observe one outstanding request for >=180 seconds'
binary = os.environ.get('REC_BIN', os.environ['CARGO_TARGET_DIR'] + '/release/rec-rs')
processes = []
failures = []

def check(condition, message):
    if not condition:
        failures.append(message)
        print('FAIL ' + message, flush=True)

with tempfile.TemporaryDirectory(prefix='rec-pending-') as tmp:
    root = pathlib.Path(tmp)
    runtime = root / 'run'
    runtime.mkdir(mode=0o700)
    config = root / 'config'
    config.mkdir()
    env = dict(os.environ, XDG_RUNTIME_DIR=str(runtime), XDG_CONFIG_HOME=str(config),
               XDG_STATE_HOME=str(root / 'state'), XDG_CACHE_HOME=str(root / 'cache'),
               XDG_CURRENT_DESKTOP='sway', XDG_SESSION_TYPE='wayland',
               WLR_BACKENDS='headless', WLR_LIBINPUT_NO_DEVICES='1',
               WLR_RENDERER='pixman', WLR_HEADLESS_OUTPUTS='1', REC_DEBUG='1')
    for key in ('DISPLAY', 'WAYLAND_DISPLAY', 'SWAYSOCK'):
        env.pop(key, None)

    def spawn(args, log):
        with (out / log).open('w') as f:
            p = subprocess.Popen(args, env=env, stdout=f, stderr=subprocess.STDOUT)
        processes.append(p)
        return p

    def run(args, timeout=2):
        return subprocess.check_output(args, env=env, stderr=subprocess.STDOUT, timeout=timeout).decode()

    def events():
        return [json.loads(line) for line in (out / 'portal.jsonl').read_text().splitlines()]

    def control(command):
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(1)
            s.connect(str(runtime / 'rec-rs-control.sock'))
            s.sendall((command + '\n').encode())
            assert s.recv(128) == b'ok\n', command

    try:
        swayconf = config / 'sway.conf'
        swayconf.write_text('output HEADLESS-1 resolution 1280x900\ndefault_border none\n')
        sway = spawn(['sway', '--unsupported-gpu', '-c', str(swayconf)], 'sway.log')
        end = time.monotonic() + 10
        while time.monotonic() < end and not list(runtime.glob('wayland-*.lock')):
            assert sway.poll() is None, (out / 'sway.log').read_text()
            time.sleep(.02)
        env['WAYLAND_DISPLAY'] = next(runtime.glob('wayland-*.lock')).name[:-5]
        env['SWAYSOCK'] = str(next(runtime.glob('sway-ipc.*.sock')))
        fake = spawn(['python3', str(pathlib.Path(__file__).with_name('fake-pending-portal.py'))], 'portal.jsonl')
        end = time.monotonic() + 10
        while time.monotonic() < end and not any(e['event'] == 'ready' for e in events()):
            assert fake.poll() is None, (out / 'portal.jsonl').read_text()
            time.sleep(.02)
        assert any(e['event'] == 'ready' for e in events()), 'fake portal failed to own names'
        settings = out / 'settings.json'
        settings.write_text(json.dumps(dict(output_dir=str(out), resolution='720p', fps=30,
                                           mic_enabled=False, system_audio=False, scene='screen')))
        env['REC_SETTINGS'] = str(settings)
        started = time.monotonic()
        app = spawn([binary, 'gui'], 'app.log')
        seen = {}
        sni_verified = False
        window_verified = False
        while time.monotonic() - started < 2:
            assert app.poll() is None, (out / 'app.log').read_text()
            for e in events():
                seen.setdefault(e['event'], e)
            if 'sni' in seen and not sni_verified:
                item = seen['sni']
                service = item['service'] if not item['service'].startswith('/') else item['sender']
                path = item['service'] if item['service'].startswith('/') else '/StatusNotifierItem'
                props = run(['gdbus', 'call', '--session', '--dest', service, '--object-path', path,
                             '--method', 'org.freedesktop.DBus.Properties.GetAll', 'org.kde.StatusNotifierItem'])
                (out / 'sni.txt').write_text(props)
                assert 'rec-rs' in props, props
                sni_verified = True
                print(f'PASS live SNI properties at {time.monotonic() - started:.3f}s', flush=True)
            if 'Start' in seen and not window_verified:
                tree = json.loads(run(['swaymsg', '-t', 'get_tree', '-r']))
                (out / 'initial-tree.json').write_text(json.dumps(tree, indent=2))
                def windows(node):
                    return ([node] if node.get('pid') == app.pid else []) + [w for n in node.get('nodes', []) + node.get('floating_nodes', []) for w in windows(n)]
                if windows(tree):
                    run(['grim', str(out / 'waiting-within-2s.png')])
                    window_verified = True
                    print(f'PASS visible window screenshot at {time.monotonic() - started:.3f}s (inspect wait text)', flush=True)
            if sni_verified and window_verified:
                break
            time.sleep(.02)
        if not window_verified:
            run(['grim', str(out / 'waiting-within-2s.png')])
        check(sni_verified and window_verified and time.monotonic() - started <= 2, 'SNI and rendered window ready within 2 seconds')
        check(window_verified, 'window visible while Start is pending')
        assert 'Start' in seen, (out / 'portal.jsonl').read_text()
        pending_since = seen['Start']['time']
        next_poke = time.monotonic()
        last_signature = None
        while time.monotonic() - pending_since < duration:
            assert app.poll() is None, (out / 'app.log').read_text()
            es = events()
            starts = [e for e in es if e['event'] == 'Start']
            creates = [e for e in es if e['event'] == 'CreateSession']
            closes = [e for e in es if e['event'] == 'Close']
            signature = (len(creates), len(starts), len(closes))
            if signature != last_signature:
                print(f'OBSERVE {time.monotonic() - pending_since:.3f}s: CreateSession={len(creates)} Start={len(starts)} Close={len(closes)}', flush=True)
                last_signature = signature
            if time.monotonic() >= next_poke:
                control('start-recording')
                control('start-recording')
                control('show')
                next_poke = time.monotonic() + 5
            time.sleep(.05)
        es = events()
        check(sum(e['event'] == 'CreateSession' for e in es) == 1, 'exactly one CreateSession over >=180s despite repeated Start')
        check(sum(e['event'] == 'Start' for e in es) == 1, 'exactly one Start over >=180s despite repeated Start')
        check(not any(e['event'] == 'Close' for e in es), 'original portal request/session remains outstanding for >=180s')
        log = (out / 'app.log').read_text(errors='replace')
        # Errors can live only in UI state rather than stderr. A replacement
        # Start at ~120s also demonstrates timeout/retry of the original.
        starts = [e for e in es if e['event'] == 'Start']
        timed_retry = len(starts) > 1 and 119 <= starts[1]['time'] - pending_since <= 123
        check('did not answer within 120 s' not in log and not timed_retry,
              'no 120-second portal Start timeout/retry')
        run(['grim', str(out / 'waiting-after-180s.png')])
        quit_started = time.monotonic()
        control('quit')
        code = app.wait(timeout=2)
        check(code == 0, f'quit exit status {code}')
        close_paths = [e['path'] for e in events() if e['event'] == 'Close']
        check(seen['Start']['path'] in close_paths, 'quit closes the pending portal Request')
        check(any('/session/' in path for path in close_paths), 'quit closes the portal Session')
        log = (out / 'app.log').read_text(errors='replace')
        check('event loop finished' in log and 'app dropped' in log, 'quit finishes event loop and drops app')
        check(not (runtime / 'rec-rs.pid').exists(), 'quit removes pidfile')
        print(f'PASS quit while waiting: {time.monotonic() - quit_started:.3f}s', flush=True)
        print(f'Observation: {time.monotonic() - pending_since:.3f}s; artifacts: {out}', flush=True)
        assert not failures, '; '.join(failures)
        print('PASS never-answering portal: one outstanding request >=180s', flush=True)
    finally:
        for p in reversed(processes):
            if p.poll() is None:
                p.terminate()
        for p in reversed(processes):
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()
