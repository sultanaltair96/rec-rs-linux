#!/usr/bin/env python3
"""Check a captured tone or beep train: level, pitch and beep spacing.

The phase 1B tests play a known 1 kHz sine (continuous or 40 ms beeps once a
second) into a PipeWire sink or a virtual microphone and record it. This script
decodes an audio track of the recording with ffmpeg and measures what actually
arrived:

- beeps: count, peak level per beep, median spacing and the median level;
- tone: the loudest 200 ms window's level and its zero-crossing frequency.

A pitch error catches a resampler that changes the rate; a wrong spacing catches
a dropped or duplicated stretch; a missing tone catches a device that was not
captured at all.

Usage: tonecheck.py FILE [--beeps] [--audio 0] [--expect-db -6] [--tol-db 4]
                         [--freq 1000] [--tol-pct 2] [--every 1.0]
                         [--min-beeps 2] [--min-db -20]

Exit status 1 on failure. Its own tests: python3 -m doctest scripts/audio/tonecheck.py
"""
import argparse
import math
import struct
import subprocess
import sys

RATE = 48000
BEEP_SECS = 0.04


def samples(path, idx):
    raw = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", path, "-map", f"0:a:{idx}",
         "-af", "pan=mono|c0=c0", "-ar", str(RATE), "-f", "f32le", "-"],
        check=True, capture_output=True).stdout
    return struct.unpack(f"<{len(raw) // 4}f", raw)


def db(level):
    return 20.0 * math.log10(level) if level > 1e-12 else -140.0


def onsets(s, threshold=0.02, gap=0.2):
    """Indices where a beep starts: above the threshold after a quiet gap."""
    out, last = [], -RATE
    for i, v in enumerate(s):
        if abs(v) > threshold:
            if i - last > gap * RATE:
                out.append(i)
            last = i
    return out


def beep_stats(s):
    """(levels_dBFS, spacing_seconds) for every beep in `s`."""
    starts = onsets(s)
    levels = []
    for i in starts:
        i1 = min(len(s), i + int(BEEP_SECS * RATE))
        levels.append(db(max((abs(v) for v in s[i:i1]), default=0.0)))
    spacing = [(b - a) / RATE for a, b in zip(starts, starts[1:])]
    return levels, spacing


def tone_stats(s):
    """(peak_dBFS, frequency_Hz) of the loudest 200 ms window."""
    win = int(0.2 * RATE)
    if len(s) < win:
        win = len(s)
    if win <= 0:
        return -140.0, 0.0
    best, best_i = -1.0, 0
    for i in range(0, len(s) - win + 1, win // 4 or 1):
        peak = max((abs(v) for v in s[i:i + win]), default=0.0)
        if peak > best:
            best, best_i = peak, i
    w = s[best_i:best_i + win]
    crossings = sum(1 for a, b in zip(w, w[1:]) if (a < 0.0) != (b < 0.0))
    return db(best), crossings / 2.0 / (len(w) / RATE)


def median(xs):
    xs = sorted(xs)
    if not xs:
        return 0.0
    return xs[len(xs) // 2]


def check_beeps(s, a):
    levels, spacing = beep_stats(s)
    ok = True
    if len(levels) < a.min_beeps:
        print(f"beeps: found {len(levels)}, need {a.min_beeps}")
        return False
    level = median(levels)
    if abs(level - a.expect_db) > a.tol_db:
        print(f"beep level {level:.1f} dBFS, expected {a.expect_db:g} +/- {a.tol_db:g}")
        ok = False
    if spacing:
        every = median(spacing)
        if abs(every - a.every) > 0.15:
            print(f"beep spacing {every:.3f} s, expected {a.every:g} s")
            ok = False
        if a.max_gap > 0 and max(spacing) > a.max_gap:
            print(f"beep gap {max(spacing):.3f} s, at most {a.max_gap:g} s allowed")
            ok = False
    else:
        every = float("nan")
    print(
        f"beeps: {len(levels)} at {level:.1f} dBFS (median), "
        f"spacing {every:.3f} s (max {max(spacing, default=0.0):.3f}), "
        f"levels {', '.join(f'{x:.1f}' for x in levels)}"
    )
    return ok


def check_tone(s, a):
    level, freq = tone_stats(s)
    ok = level >= a.min_db and abs(freq - a.freq) <= a.freq * a.tol_pct / 100.0
    print(
        f"tone: {level:.1f} dBFS at {freq:.1f} Hz "
        f"(need >= {a.min_db:g} dBFS, {a.freq:g} Hz +/- {a.tol_pct:g}%)"
    )
    return ok


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--beeps", action="store_true", help="beep train instead of a continuous tone")
    ap.add_argument("--audio", type=int, default=0)
    ap.add_argument("--expect-db", type=float, default=-6.0)
    ap.add_argument("--tol-db", type=float, default=4.0)
    ap.add_argument("--freq", type=float, default=1000.0)
    ap.add_argument("--tol-pct", type=float, default=2.0)
    ap.add_argument("--every", type=float, default=1.0)
    ap.add_argument("--min-beeps", type=int, default=2)
    ap.add_argument("--min-db", type=float, default=-20.0)
    ap.add_argument("--max-gap", type=float, default=0.0,
                    help="fail when two beeps are further apart than this (s); 0 = off")
    a = ap.parse_args()
    try:
        s = samples(a.file, a.audio)
    except subprocess.CalledProcessError as e:
        print(f"tonecheck: no decodable audio track {a.audio} in {a.file}: {e.stderr.decode(errors='replace').strip()}")
        sys.exit(1)
    ok = check_beeps(s, a) if a.beeps else check_tone(s, a)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
