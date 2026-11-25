# Direct Mutter virtual monitor smoke test

`gnome-virtual.py` uses the **real private Mutter ScreenCast API**, not the
xdg-desktop-portal API. Run only inside the isolated `gnome-headless.sh`:

```sh
scripts/dev.sh scripts/screen/gnome-headless.sh python3 scripts/screen/gnome-virtual.py
```

Additional dependency: Fedora `python3-gobject-base` (Gio/GLib typelibs).
The current dev image does not include it. Install it in a disposable derived
container/image, not on the host. No changes to the shared Dockerfile are
required by this experiment. `pipewiresrc` and `gst-launch-1.0` are already in
the dev image.

The script holds one Gio D-Bus connection through CreateSession,
RecordVirtual, Start, PipeWireStreamAdded, frame consumption, and Stop. Calling
these through separate short-lived `gdbus` processes loses session ownership.
The consumer's caps create a new 1280x720 virtual monitor, distinct from the
1920x1080 bootstrap monitor created by `gnome-headless.sh`.

## Measured result (Fedora 43 software-rendered container)

Tested in a disposable image derived from the current dev image, installing
only `python3-gobject-base` in addition. No host display/GPU/system bus used.

```text
Mutter session=/org/gnome/Mutter/ScreenCast/Session/u1 stream=/org/gnome/Mutter/ScreenCast/Stream/u1 node=44
caps = video/x-raw, format=(string)BGRA, interlace-mode=(string)progressive, width=(int)1280, height=(int)720, framerate=(fraction)0/1, max-framerate=(fraction)60/1
Got EOS from element "pipeline0".
PASS Mutter RecordVirtual: 1 real buffer, 1280x720; EOS
```

This is one automated real-frame smoke test, **not sustained-motion or recorder
integration coverage**. Attempting fixed `framerate=30/1` failed with
`stream error: no more input formats`. Omitting framerate negotiated variable
framerate (`0/1`, max 60/1). Requesting 60 frames then timed out after 30 seconds
on the static virtual desktop; requesting one frame reached normal EOS.

This bypasses the portal entirely and proves neither GNOME picker approval,
restore-token reuse, nor rec-rs portal capture. No fake backend or consent
auto-acceptance is installed. Mutter explicitly documents this API as private
and without cross-version compatibility guarantees:
https://gitlab.gnome.org/GNOME/mutter/-/blob/main/data/dbus-interfaces/org.gnome.Mutter.ScreenCast.xml
