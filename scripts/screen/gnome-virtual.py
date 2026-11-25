#!/usr/bin/env python3
"""Private Mutter API smoke test, NOT a portal consent or rec-rs test.

Run inside gnome-headless.sh. Requires python3-gobject-base (Gio typelib),
GStreamer pipewiresrc, and the private isolated compositor/session bus.
Keeps one D-Bus connection alive throughout; separate gdbus calls cannot own
this session. No fake portal, picker auto-approval, or Shell Eval is used.
"""
import os
import subprocess
import sys
import time

from gi.repository import Gio, GLib

NAME = "org.gnome.Mutter.ScreenCast"
ROOT = "/org/gnome/Mutter/ScreenCast"
SESSION = NAME + ".Session"
STREAM = NAME + ".Stream"


def main():
    if os.environ.get("REC_GNOME_SESSION") != "1":
        raise RuntimeError("run inside scripts/screen/gnome-headless.sh")
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)

    def call(path, iface, method, args=None):
        return bus.call_sync(NAME, path, iface, method, args, None,
                             Gio.DBusCallFlags.NONE, 10000, None)

    session = call(ROOT, NAME, "CreateSession", GLib.Variant("(a{sv})", ({},))).unpack()[0]
    child = None
    subscription = None
    try:
        stream = call(session, SESSION, "RecordVirtual", GLib.Variant(
            "(a{sv})", ({"cursor-mode": GLib.Variant("u", 1)},))).unpack()[0]
        nodes = []
        subscription = bus.signal_subscribe(
            NAME, STREAM, "PipeWireStreamAdded", stream, None,
            Gio.DBusSignalFlags.NONE,
            lambda conn, sender, path, iface, signal, args: nodes.append(args.unpack()[0]))
        call(session, SESSION, "Start")
        deadline = time.monotonic() + 15
        context = GLib.MainContext.default()
        while not nodes and time.monotonic() < deadline:
            while context.pending():
                context.iteration(False)
            time.sleep(0.02)
        if not nodes:
            raise RuntimeError("no PipeWireStreamAdded within 15 seconds")
        print(f"Mutter session={session} stream={stream} node={nodes[0]}", flush=True)
        # A virtual monitor is created when the consumer negotiates its size.
        # Fixed caps are essential: without them Mutter may choose an enormous
        # preferred size. num-buffers plus timeout bounds this real-frame test.
        command = ["gst-launch-1.0", "-e", "-v", "pipewiresrc",
                   f"path={nodes[0]}", "do-timestamp=true", "num-buffers=1",
                   "!", "video/x-raw,width=1280,height=720",
                   "!", "videoconvert", "!", "fakesink", "sync=false"]
        child = subprocess.Popen(command)
        try:
            status = child.wait(timeout=30)
        except subprocess.TimeoutExpired:
            raise RuntimeError("timed out waiting for a real virtual-monitor frame")
        if status:
            raise RuntimeError(f"GStreamer failed: {status}")
        print("PASS Mutter RecordVirtual: 1 real buffer, 1280x720; EOS")
    finally:
        if child is not None and child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
        if subscription is not None:
            bus.signal_unsubscribe(subscription)
        call(session, SESSION, "Stop")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"FAIL Mutter RecordVirtual: {error}", file=sys.stderr)
        sys.exit(1)
