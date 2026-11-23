# Desktop checklist

Everything in this repository that a headless container cannot prove, in one sitting, in the order
that minimises logouts and restarts. About **30–45 minutes** for parts 1–5; part 6 needs a published
release and is done once per release, not per build.

The headless suites already prove: the writer (codecs, tracks, BT.709, CFR, sync, crash safety,
defrag), the portal pipeline against xdg-desktop-portal-wlr on headless sway, the X11 pipeline under
Xvfb, the app's window/UI/mixer/preview on lavapipe, replay and RTMP/RTMPS/WHIP against MediaMTX,
the Secret Service round trip, and the CLI surface. This list is only what needs a **real panel, a
real compositor, real hardware, a real service account or a package install**.

Each check says what to do, the exact command, what to expect, and what to save as evidence. Save
everything under one folder so the report is one archive.

```sh
export EV=$HOME/rec-desktop-check-$(date +%F)
mkdir -p "$EV"
rec-rs --version | tee "$EV/version.txt"     # expect: rec-rs 0.1.2
# the package builds' output folder (packaging/README.md)
export REC_PKG_OUT=${XDG_CACHE_HOME:-$HOME/.cache}/rec-rs-linux-packaging
```

Where a check needs the app, start it from a terminal with `REC_DEBUG=1` so its log is captured, and
use the session's own file (`app-gnome.log`, `app-kde.log`, `app-x11.log`):

```sh
REC_DEBUG=1 rec-rs gui 2>"$EV/app-gnome.log"     # Part 1 (quit it before 1.1)
```

`REC_DEBUG=1` adds the debug lines the checks refer to; the app otherwise logs little. Leave the app
running between 1.6 and 1.10 so its log is one trace.

---

## Part 1 — GNOME Wayland (≈15 min, one login)

Needs GNOME 45+ (GlobalShortcuts portal), the **AppIndicator/KStatusNotifierItem** extension
enabled for the tray, and PipeWire (default on GNOME).

### 1.1 Permissions and the screen picker

- Do: with the app **not** running, request the screen grant, then list the status.
- Command:
  ```sh
  rec-rs permissions --request 2>&1 | tee "$EV/1.1-request.txt"
  rec-rs permissions | tee "$EV/1.1-status.txt"
  ls -l "${XDG_STATE_HOME:-$HOME/.local/state}/rec-rs/portal-token" | tee -a "$EV/1.1-status.txt"
  ```
- Expect: GNOME's own "Share your screen" dialog appears **once**; the line says `granted`, the
  token exists with mode `600`.
- Evidence: the transcript, plus a photo/screenshot of the dialog. Then run `--request` again: the
  dialog must appear **again** (the port forgets the grant) and stderr says
  `rec: forgetting the saved screen-capture grant so the system asks again`.
  **This is G5 — the single most important check in this part.**

### 1.2 A monitor recording, and the grant being reused

- Do: record 5 s, twice.
- Command:
  ```sh
  rec-rs record -t 5 -o "$EV/1.2-a.mp4" 2>&1 | tee "$EV/1.2-a.log"
  rec-rs record -t 5 -o "$EV/1.2-b.mp4" 2>&1 | tee "$EV/1.2-b.log"
  rec-rs probe "$EV/1.2-a.mp4" | tee "$EV/1.2-a.probe"
  ffprobe -v error -show_entries stream=codec_name,profile,width,height,r_frame_rate,color_space,color_range -of compact "$EV/1.2-a.mp4" | tee -a "$EV/1.2-a.probe"
  ```
- Expect: the first run shows the picker (or reuses the grant from 1.1 — either is fine as long as
  it is at most once); the second run shows **no picker**. Both files play, are the monitor's native
  size (or the chosen `--resolution`), 60 fps, `bt709`/`tv`.
- Evidence: both logs, the probe, a frame: `ffmpeg -y -ss 1 -i "$EV/1.2-a.mp4" -frames:v 1 "$EV/1.2-a.png"`.

### 1.3 Cursor and clicks

- Command:
  ```sh
  rec-rs record -t 4 --no-cursor -o "$EV/1.3-nocursor.mp4" 2>&1 | tee "$EV/1.3-nocursor.log"
  rec-rs record -t 4 --clicks -o "$EV/1.3-clicks.mp4" 2>&1 | tee "$EV/1.3-clicks.log"
  ```
- Do: move the pointer across the screen and click a few times during each.
- Expect: `--no-cursor` → no pointer in the file (the screen's own cursor is hidden only for the
  stream); `--clicks` → a warning that a portal carries no button state, and the cursor **is**
  recorded without rings. No crash, file plays.
- Evidence: the two logs and one frame from each (`1.3-nocursor.png`, `1.3-clicks.png`).

### 1.4 Window capture, resizing, and region

- Do: open a window (a browser or a terminal), get its id, record it for 5 s, resize it halfway
  through.
- Command:
  ```sh
  rec-rs devices | tee "$EV/1.4-devices.txt"
  rec-rs record --window <SUBSTRING-FROM-devices> -t 5 -o "$EV/1.4-window.mp4" 2>&1 | tee "$EV/1.4-window.log"
  rec-rs record --region 100,100,800,600 -t 3 -o "$EV/1.4-region.mp4" 2>&1 | tee "$EV/1.4-region.log"
  rec-rs probe "$EV/1.4-window.mp4" | tee "$EV/1.4-window.probe"
  rec-rs probe "$EV/1.4-region.mp4" | tee "$EV/1.4-region.probe"
  ```
- Expect: the picker offers **windows**; `--window` is a hint; the file has the window's size and
  follows the resize (the output size stays what the writer chose); `--region` is 800×600 × the
  desktop scale and shows exactly that part of the screen.
- Evidence: the devices list, both logs/probes, a frame from each.

### 1.5 Two monitors and `--display`

- Do (only with two monitors): list, then record `-d 2`.
- Command:
  ```sh
  rec-rs devices | tee "$EV/1.5-devices.txt"
  rec-rs record -d 2 -t 4 -o "$EV/1.5-display2.mp4" 2>&1 | tee "$EV/1.5-display2.log"
  ```
- Expect: the picker still decides (a portal has no "monitor 2"); the log names the picker and, when
  XWayland's list identifies it, the monitor that was picked. This is the documented Wayland
  difference (G11/`--display`), not a failure.
- Evidence: the log and a frame.

### 1.6 Own window while recording (the portal path)

- Do: start the app (`REC_DEBUG=1 rec-rs gui 2>"$EV/app-gnome.log"`), leave it open and idle, then
  press **Record** on the screen scene.
- Command: watch `"$EV/app-gnome.log"` while you do it.
- Expect: Rec's window disappears **before the first captured frame** (on Wayland the window is
  destroyed and recreated on stop, because `set_visible` is a no-op there; on X11 it is painted out),
  a desktop notification says it was hidden because a screen capture would film it, the recording
  runs, and the window comes back when you stop. The file must **not** contain Rec.
- Evidence: the notification (screenshot), the log lines, and a frame of the file.
- **G3 — this is the hide/notification/restore path that only a portal desktop can prove.**

### 1.7 Lock the screen while idle (G8)

- Do: start the app (preview on), note CPU in `top`/`htop` or `pidstat -p $(pgrep -f 'rec-rs gui') 1 5`,
  lock the session (Super+L), wait 10 s, unlock.
- Command: `grep -c 'preview frame' "$EV/app-gnome.log"` before and after locking (or watch the log
  with `tail -f`).
- Expect: preview frames stop while locked; CPU drops; unlocking resumes. With a recording running,
  the **recording keeps going** and the timer keeps counting.
- Evidence: the two counts, the CPU output, and a note of the lock/unlock times.

### 1.8 Tray, close-to-tray, and the app lifecycle

- Do: start a recording, close the window (Ctrl+W), reopen from the tray, pause/resume from the tray,
  Save Replay from the tray (with the replay buffer on), stop from the tray.
- Command: none beyond the tray; keep `"$EV/app-gnome.log"` open.
- Expect: closing the window keeps the recording and the tray (red dot + `mm:ss`) — on Wayland the
  window is destroyed and "Show Rec Window" recreates it; the **first** close shows "Rec is still
  running in the background; quit it from the tray" once (then never again); with no StatusNotifier
  host (disable the AppIndicator extension) closing quits instead; "Show Rec Window"
  brings it back with no gap in the file; the tray menu has Start/Stop, Pause/Resume, Save Replay
  (only while the buffer runs), Go Live/Stop Streaming, Show Rec Window, Settings, Reveal Last
  Recording (only with a recording), Open Recordings Folder, Check for Updates…, Quit; the title
  shows `mm:ss` and ` PAUSED` when paused, and ` LIVE` while streaming **without** a recording.
- Evidence: screenshots of the tray in each state and the app log; probe the file.

### 1.9 Global hotkeys (the portal dialog)

- Do: with the replay buffer **off**, press `Ctrl+Shift+R` three times (start, pause, stop). Then
  turn the replay buffer on in Settings and press `Ctrl+Shift+S`.
- Expect: the first `Ctrl+Shift+R` shows GNOME's shortcut dialog; accepting toggles recording.
  `Ctrl+Shift+S` is offered only while the replay buffer is on, and released when it is switched off.
  Refusing the dialog (or an already-used combination) shows `Ctrl+Shift+R unavailable` in the
  window and the tray.
- Evidence: a screenshot of the dialog and of the unavailable label; the app log.

### 1.10 Reveal Last, Open Folder, the folder picker and the accent colour

- Do: after a recording, click **Reveal Last**; then Settings → output folder → **Choose…**; pick a
  folder; record once into it. Look at the faders/switch colour.
- Expect: Nautilus opens with the file **selected** (FileManager1 `ShowItems`); the folder dialog is
  the desktop's own; the setting survives a restart; the accent colour is GNOME's accent (Settings →
  Appearance), falling back to Rec blue if the portal does not answer.
- Evidence: screenshots, and `jq .output_dir "$HOME/.config/rec-rs/settings.json"`.

### 1.11 Hotplug (G9)

- Do: with the app open, plug a USB microphone (or a headset) and then a webcam; watch the sidebar.
- Expect: the device appears within ~2 s **without** pressing ↻; if the preview was using the saved
  mic, it restarts on the new device; unplugging removes it.
- Evidence: the app log (the watcher prints the refresh) and before/after screenshots.

### 1.12 Real audio: sink latency and a beep

- Do: compare the latency line with the system's own value, then record a flash+beep.
- Command:
  ```sh
  rec-rs devices | tee "$EV/1.12-devices.txt"
  pactl list sinks | grep -E 'Name:|Latency|Sample Specification' | tee "$EV/1.12-pactl.txt"
  rec-rs record --test-pattern -t 5 -o "$EV/1.12-sync.mov" 2>&1 | tee "$EV/1.12-sync.log"
  python3 scripts/synccheck.py "$EV/1.12-sync.mov" --max-ms 20 | tee "$EV/1.12-sync.check"
  ```
- Expect: `Output latency (system audio is shifted by this): N ms` with a plausible N (compare
  `pactl`); sync mean ≤ 20 ms. A Bluetooth sink is the interesting case.
- Do (a Bluetooth microphone): `rec-rs record --test-pattern --mic <headset> --mic-offset-ms 120 -t 5 -o "$EV/1.12-btmic.mov"`;
  the mic stem must move by 120 ms against the flash, and `--mic <headset>` must select that device
  by name substring.
- Evidence: the three files, the Bluetooth capture, and `rec-rs probe` of it.

### 1.13 Real webcam over V4L2 (and the 2 s settle)

- Do: plug a UVC webcam, list it, record the test pattern with the overlay for 5 s, then record
  again immediately after unplugging it mid-recording.
- Command:
  ```sh
  v4l2-ctl --list-devices | tee "$EV/1.13-v4l2.txt"
  rec-rs devices | tee "$EV/1.13-devices.txt"
  rec-rs record --test-pattern --cam -t 5 -o "$EV/1.13-cam.mp4" 2>&1 | tee "$EV/1.13-cam.log"
  rec-rs probe "$EV/1.13-cam.mp4" | tee "$EV/1.13-cam.probe"
  rec-rs record --test-pattern --cam -t 10 -o "$EV/1.13-unplug.mp4" 2>&1 | tee "$EV/1.13-unplug.log"
  # unplug the camera ~4 s in, while the second recording runs
  ```
- Expect: `devices` lists the capture node with the card name (and not the metadata node);
  `--cam` starts after the 2 s settle (the first frames are not dark), the overlay is live and
  mirrored, the start log's camera line names the negotiated format (1080p30 MJPEG when YUYV only
  reaches 5 fps, YUYV/NV12 when it reaches 30); unplugging ends the camera and **finalises the
  file** with a `camera … stopped delivering frames` warning; the file plays. `--webcam <substring>`
  and `--webcam /dev/videoN` select the same device.
- Evidence: the lists, both logs, probes and a frame from the overlay file.
- **This is the only real-V4L2 proof; the Flatpak portal variant is 4.3.**

### 1.14 Preview quality and 4K cost on a real GPU

- Do: 1080p60 and then 4K (`-r 4k`) with a camera overlay, preview open, recording.
- Command: `pidstat -p $(pgrep -f 'rec-rs gui') 1 10 | tee "$EV/1.14-cpu.txt"` (or `top -d 1 -p $(pgrep -f 'rec-rs gui')`).
- Expect: the preview is the same frame as the file, smooth, crisp text at 2× scale; the CPU stays
  well below the headless figures; the low-power switch halves the preview without changing the
  file. On a GPU the compositor is not the bottleneck.
- Evidence: the CPU output, a screenshot of the preview and a frame from the file; compare the
  recording's own `probe` and the preview's visible overlay position.

### 1.15 Hardware encode (VA-API) and HEVC

- Do (only on a GPU host, `/dev/dri` present):
  ```sh
  REC_DEBUG=1 rec-rs record --test-pattern -t 5 -r 4k -o "$EV/1.15-va.mov" 2>&1 | tee "$EV/1.15-va.log"
  rec-rs probe "$EV/1.15-va.mov" | tee "$EV/1.15-va.probe"
  ffprobe -v error -show_entries stream=codec_name,profile -of compact "$EV/1.15-va.mov" | tee -a "$EV/1.15-va.probe"
  ```
- Expect: the log names a VA-API encoder (`vah264enc`/`vah265enc` or the `va*` family) rather than
  `x264enc`/`x265enc`; the file decodes; HEVC is Main at least (Main10 only with a 10-bit encoder —
  the log says which). A VA element that is offered but fails must fall back to software with a note.
- Evidence: the log, probe and ffprobe lines. **This and 4.5 are the only hardware-encode proofs.**

---

## Part 2 — KDE Plasma Wayland (≈8 min, second login)

KDE has the tray natively and its own GlobalShortcuts/portal implementation, so this part repeats the
checks whose implementation is desktop-specific.

### 2.1 Tray, menu and title

- Do: start the app with its own log (`REC_DEBUG=1 rec-rs gui 2>"$EV/app-kde.log"`); record; pause;
  stream (or turn the replay on); quit.
- Expect: the StatusNotifierItem is in the system tray; the same menu as 1.8; the red dot and
  `mm:ss` while recording, frozen when paused, ` LIVE` while streaming without a recording; the
  tooltip repeats it in words.
- Evidence: screenshots of each state.

### 2.2 Screen picker and window source

- Command: `rec-rs permissions --request`; then `rec-rs record -t 3 -o "$EV/2.2.mp4"`.
- Expect: KDE's own picker, once; windows **and** monitors are offered (KDE's portal supports
  windows); the token is reused afterwards; the recording is not black.
- Evidence: screenshots of the picker, the log and a frame.

### 2.3 Global hotkeys through KDE's portal

- Do: `Ctrl+Shift+R` (accept KDE's shortcut dialog), then `Ctrl+Shift+S` with the replay on.
- Expect: the same as 1.9 through KDE's dialog; a conflict shows `Ctrl+Shift+R unavailable`.
- Evidence: screenshots and the app log.

### 2.4 Lock the screen (G8, KDE path)

- Do: as 1.7, but with KDE's screen locker.
- Expect: preview frames stop while locked and resume on unlock; a recording keeps running.
- Evidence: the preview-frame counts and CPU.

### 2.5 Own window (KDE portal)

- Do: as 1.6.
- Expect: the window hides with a notification and returns on stop; the file does not contain Rec.
- Evidence: the notification screenshot and a frame.

---

## Part 3 — X11 session (≈8 min, one logout; Xorg or GNOME on Xorg)

Start the app in this session too, with its own log:

```sh
REC_DEBUG=1 rec-rs gui 2>"$EV/app-x11.log"
```

### 3.1 Devices, window capture and the own-window exclusion

- Command:
  ```sh
  rec-rs devices | tee "$EV/3.1-devices.txt"
  rec-rs record -t 5 -o "$EV/3.1-display.mp4" 2>&1 | tee "$EV/3.1-display.log"
  rec-rs record --window firefox -t 5 -o "$EV/3.1-window.mp4" 2>&1 | tee "$EV/3.1-window.log"
  ```
- Expect: `[1] id ...  WxH pixels (WxH points)` lines with no extra marker and no latency line when
  no sink reports one; `--window` captures that window; a window partly covered by another is
  captured correctly **with** a compositor (without one, the covered area is undefined — documented).
- Evidence: the devices list, logs, a frame from the window file.

### 3.2 The app does not record itself (G3, X11 exclusion)

- Do: start the app, open a display recording from the app (screen scene), keep the app window over
  a moving background (a video or a terminal scrolling).
- Expect: the app's window and preview are **painted out** of the file (filled with the desktop
  colour), not merely hidden; the file still has motion elsewhere; the app stays visible while
  recording. This is the X11 half of G3 with the **real** app, which no headless run does.
- Evidence: a frame of the file with the app's position marked (screenshot + frame), and
  `grep -i 'leaving .* own windows out' "$EV/app-x11.log"`.

### 3.3 Cursor and click ring

- Command: `rec-rs record -t 4 --clicks -o "$EV/3.3-clicks.mp4"` and click/move during it.
- Expect: the pointer is in the file and each click draws a ring that grows and fades.
- Evidence: two frames around a click.

### 3.4 Hotkeys on X11

- Do: `Ctrl+Shift+R` with no portal (X11 fallback `XGrabKey`), also with CapsLock and NumLock on.
- Expect: it toggles in all three states; a combination another app owns (or a second Rec) shows
  `Ctrl+Shift+R unavailable` in the window and the tray.
- Evidence: the app log and a screenshot of the label.

### 3.5 Rounded window corners

- Do: capture a window with rounded corners (GNOME Files on Xorg with a compositor, or a GTK app)
  with `--window`.
- Expect: the transparent corners are filled with the window's edge colour, not black or garbage.
- Evidence: a frame at a corner, enlarged.

### 3.6 DMA-BUF screen frames (the zero-copy import, needs a GPU)

The importer is written and its policy is unit-tested (`cargo test gpu::dmabuf`), but this host's
software Vulkan device has no DRM device, so `gpu::dmabuf::support()` is `false` here and the screen
source keeps negotiating shared memory. On a GPU desktop the two facts to establish are (a) whether
the device can import at all, and (b) that a DMA-BUF frame is bit-identical to the shared-memory one.

- Do, first, on a Wayland desktop with a GPU:
  ```sh
  cargo test --release gpu::dmabuf::tests::the_software_adapter_cannot_import -- --nocapture
  ```
- Expect: `dmabuf support on <your GPU> (DiscreteGpu or IntegratedGpu): import true`. If it prints
  `false`, that GPU/driver lacks `VK_EXT_external_memory_dma_buf` or
  `VK_EXT_image_drm_format_modifier` (`vulkaninfo | grep -E "external_memory_dma_buf|drm_format_modifier"`),
  and shared memory stays the right answer.
- Do, then, the end-to-end check **once the frame contract can carry a texture** (a `PixelFormat` /
  `VideoFrame` variant, which is the parent's call, not this round's): record a window with a
  test-pattern still on screen for 3 s, `--window`:
  ```sh
  REC_DEBUG=1 rec-rs record --window <id> -t 3 -o "$EV/3.6-dmabuf.mov" 2>&1 | tee "$EV/3.6.log"
  ```
- Expect: `rec: debug: screen stream format …` names a DMA-BUF buffer, the recording's first frame is
  pixel-identical to the same window captured with `REC_SCREEN_SHM=1` (shared memory forced), and the
  process copies no frame-sized buffer per frame (`/proc/<pid>/stat` utime, or `perf top`): a 4K
  screen share at 30 fps saves about 1 GB/s of memcpy.
- If it falls back: the log says which half failed — "this device cannot import DMA-BUFs" (the
  adapter, above) or "the compositor's frame contract has no texture frame yet" (the missing contract
  change).
- Evidence: the two logs, one frame from each path compared with `ffmpeg -i a -i b -lavfi psnr`, and
  the CPU time of both runs.

---

## Part 4 — Flatpak (≈8 min, inside the GNOME or KDE session)

Build (or take the release artifact) and install:

```sh
packaging/flatpak/build-flatpak.sh                    # writes $REC_PKG_OUT/rec.flatpak
flatpak install --user --reinstall "$REC_PKG_OUT/rec.flatpak"
```

### 4.1 The window and the tray without owning a D-Bus name

- Command: `flatpak run io.github.sultanaltair96.Rec 2>"$EV/4.1-app.log"`.
- Expect: the window opens; the tray appears (GNOME needs the AppIndicator extension); the preview
  shows frames. This is the sandbox's StatusNotifierItem check.
- Evidence: screenshots and the log.

### 4.2 Screen, audio, microphone and camera inside the sandbox

- Command:
  ```sh
  flatpak run --command=rec-rs io.github.sultanaltair96.Rec permissions --request
  flatpak run --command=rec-rs io.github.sultanaltair96.Rec devices | tee "$EV/4.2-devices.txt"
  flatpak run --command=rec-rs io.github.sultanaltair96.Rec record -t 5 -o "$EV/4.2.mp4" 2>&1 | tee "$EV/4.2.log"
  flatpak run --command=rec-rs io.github.sultanaltair96.Rec probe "$EV/4.2.mp4" | tee "$EV/4.2.probe"
  flatpak info --show-permissions io.github.sultanaltair96.Rec | tee "$EV/4.2-permissions.txt"
  ```
- Expect: the desktop's own ScreenCast dialog once; the picker's monitor is recorded; both audio
  stems exist; the camera is listed **through the portal** (no `--device=all` in the permissions);
  the file plays.
- Evidence: the transcript, permissions, probe and a frame.

### 4.3 The camera portal

- Command: `flatpak run --command=rec-rs io.github.sultanaltair96.Rec record --test-pattern --cam -t 5 -o "$EV/4.3-cam.mp4"`.
- Expect: the desktop's camera dialog appears on the first run; denying gives a clear error and no
  hang; granting records a live overlay. If a webcam is not available, note it and check the
  portal's camera list at least.
- Evidence: the dialog screenshot, log and a frame.

### 4.4 Stream keys inside the sandbox and a second instance

- Command:
  ```sh
  printf '%s\n' <YOUR-KEY> | flatpak run --command=rec-rs io.github.sultanaltair96.Rec stream-key twitch
  secret-tool search service 'rec-rs stream key' | tee "$EV/4.4-secret.txt"
  flatpak run --command=rec-rs io.github.sultanaltair96.Rec stream-key twitch --delete
  ```
- Expect: no echo of the key, a stored Secret Service entry under `rec-rs stream key`, a clean delete,
  and no file anywhere containing the key.
- Evidence: the transcript and `secret-tool` output (redact the secret before sharing).

### 4.5 VA-API in the sandbox (GPU host)

- Command: `REC_DEBUG=1 flatpak run --command=rec-rs io.github.sultanaltair96.Rec record --test-pattern -t 5 -r 4k -o "$EV/4.5.mov" 2>&1 | tee "$EV/4.5.log"`.
- Expect: the runtime's VA-API encoder is chosen (`/dev/dri` is in the sandbox), the file decodes.
- Evidence: the log and a probe.

---

## Part 5 — AppImage (≈5 min, same session)

```sh
packaging/appimage/build-appimage.sh        # writes $REC_PKG_OUT/Rec-<version>-x86_64.AppImage + .zsync
chmod +x "$REC_PKG_OUT"/Rec-*-x86_64.AppImage
```

### 5.1 Runs on a desktop with the host's PipeWire

- Command:
  ```sh
  "$REC_PKG_OUT"/Rec-*-x86_64.AppImage 2>"$EV/5.1-app.log"
  "$REC_PKG_OUT"/Rec-*-x86_64.AppImage record -t 5 -o "$EV/5.1.mp4" 2>&1 | tee "$EV/5.1.log"
  "$REC_PKG_OUT"/Rec-*-x86_64.AppImage probe "$EV/5.1.mp4" | tee "$EV/5.1.probe"
  ```
- Expect: the window opens; the tray appears; the screen capture works through the portal with the
  **host's** PipeWire (the bundle deliberately does not ship libpipewire); the file plays.
- Evidence: screenshots, logs and the probe.

### 5.2 VA-API with the bundled GStreamer

- Command: `REC_DEBUG=1 "$REC_PKG_OUT"/Rec-*-x86_64.AppImage record --test-pattern -t 5 -r 4k -o "$EV/5.2.mov" 2>&1 | tee "$EV/5.2.log"`.
- Expect: `vah264enc`/`vah265enc` from the host's libva driver, or the software fallback with a note
  if the driver is not usable; the file decodes.
- Evidence: the log and a probe.

### 5.3 The file in other players

- Do: open `1.2-a.mp4` and `tracks`-style output in **VLC**, **mpv** and, if installed, **Kdenlive**
  or **Shotcut**; switch the audio track in VLC (`Audio → Audio Track`).
- Expect: all tracks visible; the mix is the default, both stems are alternatives; the duration and
  A/V sync match `rec-rs probe`. (QuickTime/Final Cut need macOS and are not part of this list.)
- Evidence: screenshots of the track menus and the players' duration readouts.

---

## Part 6 — Release-time updates (once per release, ≈10 min)

Not per build: these need a published newer version.

### 6.1 `flatpak update`

- Do: publish a build with a higher version to the same remote, then
  `flatpak update io.github.sultanaltair96.Rec` and `flatpak run io.github.sultanaltair96.Rec --version`.
- Expect: the new version installs and prints it. The app's own "Check for Updates…" must be
  **absent** in the Flatpak (the software centre owns updates).
- Evidence: the transcript.

### 6.2 AppImage zsync

- Command:
  ```sh
  "$REC_PKG_OUT"/Rec-*-x86_64.AppImage --appimage-updateinformation   # gh-releases-zsync|sultanaltair96|rec-rs-linux|latest|...
  AppImageUpdate "$REC_PKG_OUT"/Rec-*-x86_64.AppImage
  ```
- Do: upload a newer AppImage and its `.zsync` to the GitHub release first.
- Expect: `AppImageUpdate` downloads only the delta and the updated image runs and prints the new
  version; without `AppImageUpdate`, the app's "Check for Updates…" opens the release page and its
  "Add to Menu" installs the desktop entry.
- Evidence: the update information, the `AppImageUpdate` transcript, `--version` before/after.

### 6.3 Real streaming services (≈15 min, can be done from the AppImage or the Flatpak)

- Do: go live to YouTube, Twitch and Kick for 30 s each (keys via `stream-key`), then all three at
  once; then HEVC to YouTube with `--stream-codec hevc`; then RTMPS against the real certificate.
- Command:
  ```sh
  rec-rs record --no-record --stream youtube --stream twitch --stream kick -t 30 2>&1 | tee "$EV/6.3-all.log"
  rec-rs record --no-record --stream youtube --stream-codec hevc -t 30 2>&1 | tee "$EV/6.3-hevc.log"
  rec-rs record --no-record --stream rtmps://a.rtmps.youtube.com/live2 -t 10 2>&1 | tee "$EV/6.3-rtmps.log"
  ```
- Expect: one encode to all three (the status line shows three `live ... Mb/s drop 0` fields), each
  service's own player in sync with a hand-clap/beep test, YouTube accepting enhanced-RTMP HEVC (or
  the documented H.264 fallback with a note), and RTMPS verifying the real certificate (no
  `REC_INSECURE_TLS` needed and, if exported, ignored with a warning naming the host).
- Evidence: the logs, screenshots of each service's health page, and the beep timing note.

### 6.4 Replay while recording and stop from a second instance

- Command:
  ```sh
  rec-rs record --replay 60 --stream twitch -t 60 2>&1 | tee "$EV/6.4-rec.log" &
  sleep 20; rec-rs save-replay; sleep 5; rec-rs pause; sleep 3; rec-rs resume
  rec-rs save-replay; rec-rs stop
  ```
- Expect: both clips are saved while recording, play in VLC/mpv, start on a keyframe, and the stream
  survives the pause/resume; the app's live settings are locked while live.
- Evidence: the recording log, both clips and `rec-rs probe` of each.

---

## What to send back

One archive of `$EV` plus:

1. the desktop and versions (`gnome-shell --version`, `plasmashell --version`, `echo $XDG_SESSION_TYPE`,
   `flatpak --version`, `uname -r`, GPU driver);
2. a PASS/FAIL line per numbered check, with the link to its evidence file;
3. anything that looked wrong, even if it is not in this list — especially a picker that appears
   twice, a black recording, a tray that never appears, a hotkey that needs a reboot, or a crash.

**Cleanup:** stop any recording, quit the app, `flatpak uninstall io.github.sultanaltair96.Rec` if it
was installed for the check, and delete `$EV` only after it has been shared.
