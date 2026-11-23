#!/usr/bin/env python3
"""Generate a test WAV for the phase 1B audio tests.

A sine at a known level, either continuous or as beeps once a second, so the
captured file can be checked for level, pitch and timing without any audio
hardware. 16-bit PCM, so every player (pw-play, paplay) can play it.

Usage: mkwav.py OUT [--rate 48000] [--secs 8] [--freq 1000] [--level -6]
                    [--beep 0.04] [--every 1.0] [--channels 2]

`--level` is dBFS of the sine (default -6, the level of the test pattern's
beep). `--beep 0` is a continuous tone.

Adapted from rec-rs scripts/synth.wav generation in scripts/selftest.sh
(MIT OR Apache-2.0).
"""
import argparse
import math
import struct
import wave


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--rate", type=int, default=48000)
    ap.add_argument("--secs", type=float, default=8.0)
    ap.add_argument("--freq", type=float, default=1000.0)
    ap.add_argument("--level", type=float, default=-6.0, help="dBFS of the sine")
    ap.add_argument("--beep", type=float, default=0.04, help="beep length in s (0 = continuous)")
    ap.add_argument("--every", type=float, default=1.0, help="beep interval in s")
    ap.add_argument("--channels", type=int, default=2, choices=(1, 2))
    a = ap.parse_args()

    amp = 10.0 ** (a.level / 20.0)
    n = int(a.rate * a.secs)
    frames = bytearray()
    for i in range(n):
        t = i / a.rate
        on = a.beep <= 0.0 or (t % a.every) < a.beep
        # A short raised-cosine fade avoids clicks at the beep edges; the peak
        # is still exactly the requested level.
        v = amp * math.sin(2.0 * math.pi * a.freq * t) if on else 0.0
        s = int(round(max(-1.0, min(1.0, v)) * 32767.0))
        for _ in range(a.channels):
            frames += struct.pack("<h", s)
    with wave.open(a.out, "wb") as w:
        w.setnchannels(a.channels)
        w.setsampwidth(2)
        w.setframerate(a.rate)
        w.writeframes(bytes(frames))
    print(
        f"mkwav: {a.out} {a.secs:g}s {a.rate} Hz {a.channels}ch "
        f"{a.freq:g} Hz at {a.level:g} dBFS"
        + (f", {a.beep * 1000:g} ms every {a.every:g}s" if a.beep > 0 else ", continuous")
    )


if __name__ == "__main__":
    main()
