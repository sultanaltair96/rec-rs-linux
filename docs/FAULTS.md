# Lifecycle fault fixtures and current results

Opt-in: `scripts/dev.sh scripts/selftest.sh faults-cli faults-gui`. These are
not part of the default selftest. Each case starts a private headless sway,
PipeWire and portal session. Results below are from the release build based on
`650ee50`; they do not include the later idle-preview change. A PASS covers
only the assertions listed here, not the complete release gate.

| Fault | Expected behaviour | Current evidence / gap |
|---|---|---|
| Screen node removed while recording | CLI and GUI stop, finalize a decodable file; GUI quit under 2 s | PASS both, sway portal; validates node identity and decoding |
| PipeWire killed while recording | Stop and finalize, or recover without hanging | **FAIL** both: CLI did not exit in 30 s; GUI recording did not finalize in 30 s |
| wlr portal backend killed | Stop/finalize or continue correctly | PASS both in this harness; does not prove GNOME portal restart or idle-preview recovery |
| Synthetic node error, EOS, format change | No hang; stop/finalize or recover | SKIP: no deterministic portal producer injection |
| Dock burst: screen, camera and default sink lost and restored in either order | Capture stops/finalizes or recovers correctly; idle preview recovers, one outstanding request | NOT IMPLEMENTED: `run.sh dock-*` currently exercises only synthetic audio sink/mic, not the three-device dock burst or default-sink replacement; its PASS is fixture-only |
| PipeWire and WirePlumber restart | Consumers recover or stop/finalize; GUI quits under 2 s | Fixture-only in isolated `session.sh`; recorder and GUI recovery not tested |
| Portal frontend restart | Same | NOT IMPLEMENTED; existing portal-stop case kills the wlr backend instead |
| PipeWire SIGSTOP 30 s / SIGCONT | No hang; recover or stop/finalize; GUI quits under 2 s | Fixture-only, not coupled to recorder or GUI assertions |
| ENOSPC | Writer reports failure without hang; retain playable data where possible | Owned by phase 1G (writer). The namespace/tmpfs fixture was removed from this harness; no ENOSPC coverage is claimed here |
| v4l2loopback hotplug | Camera disappears/returns safely | NOT TESTED: no module/host access; fixture offers a synthetic PipeWire camera instead, not V4L2 hotplug |
| GNOME lock, GNOME portal picker, restore grant, session Closed, monitor unplug | No hang, one outstanding picker, correct recovery/finalization | NOT TESTED: GNOME Shell and GNOME portal start headlessly, but no automated picker consent or saved grant. `--virtual-monitor` creates the initial monitor; runtime removal/re-add was not established. No simulated lock or fake picker is claimed |

## Selftest output safety

`scripts/selftest.sh` takes a nonblocking exclusive `flock` on
`${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}/.selftest.lock` before deleting or
creating its worktree-specific output tree. Concurrent runs sharing that base
fail immediately with `selftest: REC_TEST_DIR is busy`, preserving the active
run's evidence. Use separate `REC_TEST_DIR` values for independent runs. The
lock file is persistent: do not delete it while a run is active. The descriptor
is held for the run and inherited by children, so surviving children also keep
output protected; process exit releases it without stale PID-file cleanup.
`--list` does not lock or clear output.

Container verification with a blocking mock recorder: **5 passed, 0 failed**:
concurrent same-directory invocation rejected without deleting a sentinel;
`--list` preserved it while locked; a distinct directory ran concurrently;
after the first runner exited, the next run acquired the lock and cleared its
output; `sh -n scripts/selftest.sh` passed. This tests harness exclusion, not
recording or writer fault handling.

Older ENOSPC invocation examples in
`scripts/faults/README.sh` are obsolete; writer fault coverage belongs to 1G.

`scripts/screen/gnome-headless.sh` starts actual GNOME Shell/Mutter, its
ScreenCast portal, a private bus and PipeWire. Portal property readiness is
**not** proof of frame capture or restore-token behaviour. The scripts retain
logs outside the repository. Synthetic audio fixture and camera results only
prove the devices were advertised, not application recovery. Existing tests
do not yet count outstanding portal requests or assert idle preview recovery.
