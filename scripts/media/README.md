# Opt-in disk faults

```sh
scripts/dev.sh cargo build --release
scripts/dev.sh bash scripts/media/disk-faults.sh --run
```

Without `--run`, the fixture prints SKIP. It first attempts a real tmpfs mount
in an unprivileged user/mount namespace; disabled user namespaces or container
mount restrictions produce SKIP, not a false pass. No sudo, Docker privilege
changes, host mounts, or real disk exhaustion are used. Do not run `--inside`
directly. Normal `scripts/selftest.sh` runs are unchanged.

Each case mounts a private 32 MiB tmpfs and starts a bounded test-pattern
recording. Only after a complete moof/mdat fragment exists does it inject:

- **ENOSPC:** a filler consumes the remaining tmpfs space. Its write must report
  ENOSPC, and the recorder must exit nonzero (not time out).
- **Output folder disappearance:** rename the output directory away while the
  recording is open. The requested path must disappear. Linux keeps the open
  inode alive; either successful completion or a reported finalization error
  is acceptable, but hanging or losing/changing completed fragments is not.
  This deliberately does not unlink the recording or simulate device removal.

Both cases require byte-identical previously completed moof/mdat fragments and
an ffmpeg video decode of the recoverable prefix. A raw copy (including any
incomplete tail), recovered prefix, fragment hashes, recorder status, and logs
are retained under a unique directory in
`${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}`. A trailing incomplete fragment is
excluded only from the recovered copy, never modified in the raw evidence.
Mounts and child processes are cleaned up on exit. Settings/runtime paths are
isolated per run. The fixture does not build automatically; `REC` may select an
existing binary. A restricted-container SKIP is not evidence that the two real
fault cases passed.
