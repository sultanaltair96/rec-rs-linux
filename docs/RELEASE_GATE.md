# Real-hardware release gate

**Gate: two consecutive clean passes, independently for each exact release artifact.** A failure, crash, unexpected warning, missing evidence, skipped mandatory case, package/hash change, or fix resets that artifact's count to zero. Repeat the entire checklist twice after the last change. Do not ship with an open or waived failure. This document specifies checks, not results.

## Setup and safety

Run on **agent-amd: Fedora/GNOME 49 Wayland, AMD iGPU, USB-C/MST dock with display, webcam and USB audio**, with an external second monitor and Bluetooth headset. Record unavailable prerequisites as BLOCKED, not PASS. The user must approve disruptive tests and be present for physical actions; the remote orchestrator may run commands and collect sanitized evidence but must not lock, suspend, restart PipeWire, fill a filesystem, disconnect devices, alter system settings or stream publicly without explicit approval at execution time. Preserve work before each disruption. Schedule the 2-hour test and 10-minute stream separately; the entire gate is not a 45-minute smoke test.

Install the exact candidate Flatpak bundle from the release staging location; use the exact candidate AppImage, not a development binary or a previously installed build. The operator supplies absolute paths and a private output folder **under Videos** (accessible to the sandbox). Run each numbered case twice per artifact, once in each complete pass; do not interleave different builds. Set `APPIMAGE` to the precise filename (no wildcard); the Flatpak install may replace an installed candidate only with operator consent.

```sh
export EV="$HOME/Videos/rec-release-gate/$(date +%F-%H%M%S)"
mkdir -p "$EV"
export BUNDLE=/absolute/path/to/rec.flatpak
export APPIMAGE=/absolute/path/to/Rec-candidate-x86_64.AppImage
sha256sum "$BUNDLE" "$APPIMAGE" | tee "$EV/artifacts.sha256"
flatpak install --user --reinstall "$BUNDLE"
chmod +x "$APPIMAGE"
flatpak info --show-commit --show-permissions io.github.sultanaltair96.Rec > "$EV/flatpak-info.txt"
flatpak run --command=rec-rs io.github.sultanaltair96.Rec --version | tee "$EV/flatpak-version.txt"
"$APPIMAGE" --version | tee "$EV/appimage-version.txt"
printf '%s\n' "$XDG_SESSION_TYPE" > "$EV/session.txt"
gnome-shell --version >> "$EV/session.txt"; uname -a >> "$EV/session.txt"
```

Verify the hashes match the staged release manifest; record source commit, package version, Flatpak commit, GPU/driver (`lspci -nnk`), dock/monitor topology (`gnome-randr` if installed, otherwise GNOME Settings screenshot), camera/mic model, and operator. Store logs, screenshots, player/probe output and sample recordings under `$EV`, never in the repository. Do not capture passwords, stream keys, portal tokens, sensitive windows or private audio; redact before sharing.

For each artifact, choose one invocation (do not use the host `rec-rs` binary):

```sh
# Flatpak pass:
rec() { flatpak run --command=rec-rs io.github.sultanaltair96.Rec "$@"; }
# AppImage pass (replace the function above):
# rec() { "$APPIMAGE" "$@"; }
rec --version
```

Use distinct `$EV/<pass>-<case>` names each run. `rec gui` starts the app; run it in a terminal with `REC_DEBUG=1 rec gui 2>"$EV/app.log"` and retain that log. Avoid a second app instance when exercising the CLI: quit the GUI unless a case explicitly requires coexistence. `rec probe FILE` takes one file. Save raw `ffprobe -v error -show_streams -show_format -of json FILE` for each recording. Full decode: `ffmpeg -v error -i FILE -f null -`. Record observed start/end times, frame counts (`ffprobe -v error -select_streams v:0 -count_frames -show_entries stream=nb_read_frames,r_frame_rate,duration -of json FILE`), any drops, and visual/audio observations. `[ ]` is filled PASS/FAIL/BLOCKED and linked to evidence; an empty box is not a pass. Expected recovery is a **release requirement**, not an assertion that current code already implements it.

## Numbered execution record — repeat for Flatpak and AppImage

| # | Action / command | Expected result | Verify / evidence | Physical user | Result |
|---|---|---|---|---|---|
| 1 | Fresh grant: run `rec permissions --request`; select the dock display in Share. Then `rec record --no-mic --no-system-audio -t 5 -o "$EV/grant.mp4"`; repeat to `restore.mp4`. | Picker is shown for the forced request; grant succeeds; subsequent recordings restore the grant without another dialog and show moving content. | Capture sanitized terminal log, screenshots of picker and second run, probe and decode both clips; note if desktop declines restore. | Yes: Share click | [ ] |
| 2 | `REC_DEBUG=1 rec gui 2>"$EV/app.log"`; accept GlobalShortcuts prompt; Ctrl+Shift+R twice; close the window, reopen via tray, then quit via tray. | Tray appears, hotkey starts/stops, close leaves app resident, reopening works, Quit exits and finalizes any recording. | Screenshot tray idle/recording/closed, log and playable output; check process exits. | Yes: prompt and visible interaction | [ ] |
| 3 | In GUI select Screen, Screen + Cam, Cam in turn; watch each preview for at least 30 s before and during recording. | Preview steady, current and not flickering/black in all scenes; no repeated camera reopening. | Screen recording or timestamped observer notes, app log and captured clips; compare visible preview to clip. | Yes: visual observation | [ ] |
| 4 | Record screen, screen+cam, cam and a chosen window, 15 s each, from GUI; record a region using `rec record --region 100,100,800,600 -t 15 -o "$EV/region.mp4"` after closing GUI. | Each contains the intended subject, correct crop, changing frames, clean termination and playable file; camera-only contains camera. | Probe, full decode, representative beginning/middle/end frames and screenshot of chosen window/region. | Yes: select Share/window | [ ] |
| 5 | With working mic and speaker, play a known tone and speak distinct words while recording screen+cam; make separate H.264 and HEVC files: `rec record --codec h264 -t 15 -o "$EV/h264.mov"` and `rec record --codec hevc -t 15 -o "$EV/hevc.mov"`. | Both codecs decode; system and mic are audible in appropriate stems and mix, no unexpected clipping; sync is acceptable. | ffprobe codec/profile/streams, full decode, listen to each track; record measured clap/flash offset and audio device names. | Yes: speak/play tone and picker | [ ] |
| 6 | During screen+cam recording move overlay, switch corners, size, shape, border, shadow and mirror. | Preview and subsequent recorded frames follow every change live without restart or flicker. | Screenshots before/after and extracted frames with timestamps; probe/decode file. | Yes: GUI manipulation | [ ] |
| 7 | With GUI idle and preview running, physically unplug the **dock**, wait 15 s, replug, wait 60 s. | App remains responsive and preview plus dock display/camera/audio automatically reconnect without restarting app. | Video/screenshots, `app.log`, `rec devices` before/after. Prior agent-amd `beb11fb` failed idle recovery; `1cf9cc0` remains unproven on device. | Yes: dock cable | [ ] |
| 8 | Start a dock-display recording; unplug dock after 30 s; replug after recording ends. | Recording stops cleanly and finalizes a playable file; app/tray survives; devices and preview recover on replug. | App log, stop time, probe/full decode and end frame; distinguish finalize success from reconnect. Prior `beb11fb` produced playable 2:34; replug recovery not proved. | Yes: dock cable | [ ] |
| 9 | Disconnect and reconnect **only the monitor**, not dock, first idle then during a 30 s capture. | No crash; capture stops/finalizes or recovers cleanly, and preview/device list auto-recovers after reconnect. | App log, timestamps, probe/decode and before/after display list. | Yes: display cable | [ ] |
| 10 | During a screen+cam recording unplug the webcam; repeat with USB microphone. Reconnect each while app remains open. | Device loss is clearly reported; session remains responsive and recording finalizes; device reappears and next session can use it. | Logs, devices before/after, two clips with probe/decode and audio listening. | Yes: USB unplug/replug | [ ] |
| 11 | Hold camera open in another camera application, then select it in Rec and start capture; release it and retry. | A clear busy/unavailable error, no hang/crash; after release camera is usable. | Both app logs, screenshot of error and successful retry clip. | Yes: launch/close other app | [ ] |
| 12 | While recording system audio, change default output from dock to another sink, then switch Bluetooth headset A2DP ↔ HFP and back with a call/test app. | No hang or crash; document whether capture follows the new sink (current documented behavior may require restarting); new session captures selected sink and mic after each switch. | `pactl info`, `pactl list short sinks`, logs before/after; listen to distinct tones and probe each clip. A silent track without warning is FAIL. | Yes: select sinks and Bluetooth mode | [ ] |
| 13 | Record with tray visible; lock GNOME, wait 30 s, unlock with password, stop. Repeat while idle. | Tray survives; preview stops updating while locked and resumes; recording continues and file plays across lock. | Screenshots before/after, app log/time markers, probe/decode and video around lock interval. Never record password. | Yes: lock and password | [ ] |
| 14 | Suspend with lid while idle; wait 30 s, resume/unlock; repeat during a 60 s recording. | No crash; tray and preview recover; recording resumes or stops with a clear notice and finalizes a playable file, without a hang. | `journalctl --user --since ...` sanitized, app log, timestamps, file probe/decode. | Yes: lid and unlock | [ ] |
| 15 | Set GNOME fractional scaling to 125%, then 150%; at each scale run `rec record --region 100,100,800,600 -t 10 -o "$EV/region-SCALE.mp4"` (replace SCALE). | Captured pixels correspond to the requested logical rectangle, not a shifted/cropped desktop; dimensions reflect negotiated scaling and are even. | Screenshot marked rectangle, ffprobe dimensions and frame comparison at each scale; restore original scale. | Yes: GNOME Settings/picker | [ ] |
| 16 | Attach two monitors at different scales; choose each in picker and repeat region capture on each. | Picker selects requested monitor; region maps to correct content on each, with no wrong-monitor capture. | Display settings screenshot, picker screenshots, logs, dimensions and marked frames from both clips. | Yes: display setup and picker | [ ] |
| 17 | **Only after approval**, create a disposable small filesystem outside the repo and direct recording there (for Flatpak grant access to that isolated path if needed). Record with little free space, then fill it completely while recording. Never fill the user's real home or system filesystem. | Low-space condition is visible; full disk gives clear error, app remains responsive, no silent successful recording, and recoverable output is finalized if possible. | Record filesystem size/free before/after (`df -h`), app log, exit status, probe/decode partial file; cleanup only disposable filesystem. | Yes: approve isolated filesystem and monitor | [ ] |
| 18 | Record 2 hours with screen+cam and both audio sources; make a visible timestamp and audible sync marker at start/end. Sample process RSS every minute using `pidstat -r -p PID 60 121` (replace PID), and keep logs. | Duration ≈7200 s; RSS bounded with no sustained growth; no frozen screen, unacceptable drops, track loss or accumulating A/V drift. | RSS samples, ffprobe duration/frame counts, logs, start/middle/end frames and measured start/end sync; retain full artifact or checksum and excerpts. | Yes: setup and start/end sync markers | [ ] |
| 19 | **After approval**, restart the user's PipeWire service during idle preview, then separately during recording: `systemctl --user restart pipewire.service pipewire-pulse.service` (ensure WirePlumber returns). | No permanent hang/crash; interruption is reported, recording finalizes or recovers cleanly, preview and devices recover without app restart. | `systemctl --user status pipewire pipewire-pulse wireplumber`, sanitized journal, app log and probe/decode; restore audio session if needed. | Yes: approve disruption, verify audio restored | [ ] |
| 20 | With explicit consent and a private/test real RTMP endpoint, stream for 10 min: `rec record --no-record --stream "$RTMP_DEST" -t 600 2>"$EV/stream.log"`. Set key securely via `rec stream-key "$RTMP_DEST"` interactively; never put it in a command, log or report. | Destination stays live for 10 min with bounded drops, audio/video present and in sync; no secret leaks. | Redacted CLI status, service health screenshot and time-aligned clap/flash measurement; delete test credential with consent. | Yes: credentials, service and approval | [ ] |
| 21 | Produce a diagnostic report for the pass: `printf 'artifact hashes, version, session, device topology, pass results and evidence paths: see attached files\n' > "$EV/report.txt"`; append sanitized logs, `flatpak-info.txt`, `artifacts.sha256`, probe summaries and PASS/FAIL/BLOCKED for every row. | Report is complete and reproducible; no tokens, keys, passwords or private content. A product-generated diagnostic command is not assumed. | Inspect/redact the report and attachments; record path and SHA-256. | Yes: privacy review | [ ] |

## Pass ledger

| Artifact | SHA-256 / version / source commit | Pass 1 date + evidence | Pass 2 date + evidence | Gate |
|---|---|---|---|---|
| Flatpak | not yet | not yet | not yet | BLOCKED |
| AppImage | not yet | not yet | not yet | BLOCKED |

An operator and the remote orchestrator sign each pass with date and evidence location. A later failure or artifact change resets both entries for that artifact. Do not convert a prior source-build result into a release-package pass.
