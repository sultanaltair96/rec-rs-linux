#!/usr/bin/env python3
"""Run via scripts/dev.sh dbus-run-session -- python3 scripts/screen/gui-sleep.py.

Requires built release binary (or REC_BIN). Private fake *system* bus and Xvfb;
never suspends the host. Artifacts persist outside the repository.
"""
import json
import os
from pathlib import Path
import socket
import subprocess as sp
import tempfile
import time

binary = os.environ.get('REC_BIN', os.environ.get('CARGO_TARGET_DIR', '') + '/release/rec-rs')
base = Path(os.environ.get('REC_TEST_DIR', str(Path.home() / 'Videos/rec-rs-tests')))
base.mkdir(parents=True, exist_ok=True)
out = Path(tempfile.mkdtemp(prefix='gui-sleep-', dir=base))
print(f'Artifacts: {out}', flush=True)
processes = []
with tempfile.TemporaryDirectory(prefix='rec-sleep-') as tmp:
    root = Path(tmp)
    env = dict(os.environ, XDG_RUNTIME_DIR=tmp, XDG_SESSION_TYPE='x11', REC_DEBUG='1',
               REC_SETTINGS=str(out / 'settings.json'), XDG_CONFIG_HOME=str(root / 'config'))
    env.pop('WAYLAND_DISPLAY', None)

    def spawn(args, log, **kwargs):
        with (out / log).open('w') as f:
            p = sp.Popen(args, env=env, stdout=f, stderr=sp.STDOUT, **kwargs)
        processes.append(p)
        return p

    def wait(predicate, label, seconds=30):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            assert all(p.poll() is None for p in processes), f'process exited: {label}'
            result = predicate()
            if result:
                return result
            time.sleep(.02)
        raise AssertionError('timeout: ' + label)

    def events():
        return [json.loads(s) for s in (out / 'logind.jsonl').read_text().splitlines()]

    def seen(kind, seq):
        return any(e['event'] == kind and e.get('id') == seq for e in events())

    def control(command):
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(2)
            s.connect(str(root / 'rec-rs-control.sock'))
            s.sendall((command + '\n').encode())
            assert s.recv(128) == b'ok\n'

    def signal(command):
        fake.stdin.write(command + '\n')
        fake.stdin.flush()

    def playable(path):
        # Decode every video frame, not merely container metadata.
        data = sp.check_output(['ffprobe', '-v', 'error', '-count_frames', '-select_streams', 'v:0',
                                '-show_entries', 'stream=nb_read_frames', '-of', 'json', str(path)], timeout=30)
        frames = int(json.loads(data)['streams'][0]['nb_read_frames'])
        assert frames >= 30, frames
        sp.run(['ffmpeg', '-v', 'error', '-xerror', '-i', str(path), '-f', 'null', '-'],
               check=True, timeout=30, stdout=sp.DEVNULL, stderr=sp.PIPE)
        return frames

    try:
        # daemon's stdout is a dedicated address pipe, never a host bus address.
        bus = sp.Popen(['dbus-daemon', '--session', '--nofork', '--print-address=1'], stdout=sp.PIPE, text=True)
        processes.append(bus)
        env['DBUS_SYSTEM_BUS_ADDRESS'] = bus.stdout.readline().strip()
        assert env['DBUS_SYSTEM_BUS_ADDRESS'].startswith('unix:')
        fake = spawn(['python3', str(Path(__file__).with_name('fake-logind.py'))], 'logind.jsonl', stdin=sp.PIPE, text=True)
        wait(lambda: any(e['event'] == 'ready' for e in events()), 'fake ready')
        # -displayfd avoids collisions with parallel worktrees.
        readfd, writefd = os.pipe()
        xvfb = spawn(['Xvfb', '-displayfd', str(writefd), '-screen', '0', '1280x800x24', '-nolisten', 'tcp'],
                     'xvfb.log', pass_fds=(writefd,))
        os.close(writefd)
        import select
        assert select.select([readfd], [], [], 10)[0], 'Xvfb readiness'
        env['DISPLAY'] = ':' + os.read(readfd, 100).decode().strip()
        os.close(readfd)
        recordings = out / 'recordings'
        recordings.mkdir()
        (out / 'settings.json').write_text(json.dumps(dict(output_dir=str(recordings), resolution='720p',
                        fps=30, mic_enabled=False, system_audio=False, scene='screen')))
        app = spawn([binary, 'gui', '--test-pattern'], 'app.log')
        wait(lambda: seen('inhibit', 1) and (root / 'rec-rs-control.sock').exists(), 'idle inhibitor')
        signal('sleep')
        wait(lambda: seen('released', 1), 'idle release')
        assert not list(recordings.iterdir()), 'idle created recording'
        signal('resume')
        wait(lambda: seen('inhibit', 2), 'idle resume inhibitor')
        control('show')
        print('PASS idle sleep release, resume renewal, GUI control', flush=True)
        control('start-recording')
        wait(lambda: list(recordings.glob('*.mov')), 'recording file')
        time.sleep(3)
        assert not seen('released', 2), 'inhibitor released during recording'
        signal('sleep')
        wait(lambda: seen('released', 2), 'recording inhibitor release')
        released = next(e['time'] for e in events() if e['event'] == 'released' and e['id'] == 2)
        files = list(recordings.glob('*.mov'))
        assert len(files) == 1
        frames = playable(files[0])
        assert not (root / 'rec-rs-recorder.sock').exists(), 'recorder still active after release'
        print(f'PASS recording sleep release; immediately following EOF: playable {frames} frames (EOF observed at {released:.6f})', flush=True)
        signal('resume')
        wait(lambda: seen('inhibit', 3), 'recording resume inhibitor')
        time.sleep(2)
        assert len(list(recordings.glob('*.mov'))) == 1
        assert not (root / 'rec-rs-recorder.sock').exists(), 'unexpected automatic restart'
        control('start-recording')
        wait(lambda: len(list(recordings.glob('*.mov'))) == 2, 'explicit recording after resume')
        time.sleep(3)
        control('stop')
        wait(lambda: not (root / 'rec-rs-recorder.sock').exists(), 'explicit stop')
        second = next(p for p in recordings.glob('*.mov') if p != files[0])
        wait(lambda: second.stat().st_size > 0, 'second file bytes')
        # Stop socket disappearance can precede finalizer completion; allow closure.
        time.sleep(2)
        frames2 = playable(second)
        assert not seen('released', 3), 'renewed inhibitor unexpectedly closed while awake'
        print(f'PASS resume renews inhibitor without auto-start; explicit restart/stop playable {frames2} frames', flush=True)
        print('RESULT 3 passed; GAP: fake logind, no physical suspend; decoding starts after observed EOF, not atomically at FD close.', flush=True)
    finally:
        for p in reversed(processes):
            if p.poll() is None:
                p.terminate()
        for p in reversed(processes):
            try:
                p.wait(timeout=5)
            except sp.TimeoutExpired:
                p.kill()
                p.wait()
