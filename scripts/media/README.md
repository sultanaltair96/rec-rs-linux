# Opt-in disk faults

```sh
scripts/dev.sh cargo build --release --locked
DEV_SMALL_TMPFS=96m scripts/dev.sh bash scripts/media/disk-faults.sh --run
```

Without `--run`, the fixture skips. Docker supplies a dedicated tmpfs at
`/mnt/rec-small`; its pages count against `DEV_MEMORY`. The fixture refuses
mounts larger than 128 MiB and isolates settings and runtime paths.

The four cases verify preflight rejection, a clean low-water stop, actual
mid-recording ENOSPC, and removal of the output directory. They measure stop
latency and decode the output or recovered complete fragments. The removal case
keeps a hard link outside the removed directory solely for inspection: without
such a link, unlinking a recording makes it unrecoverable after its open file
descriptor closes. A physical device disconnect still requires hardware testing.

Evidence is retained under `${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}`. The
fragment recovery helper copies completed moof/mdat pairs to a separate file;
it never modifies the raw recording.
