//! MP4 box surgery: read a recording's movie structure, turn a fragmented
//! recording into a regular movie **in place**, and carry the track layout
//! (alternate group, enabled flag, AAC priming edit list) into the file.
//!
//! # Why a fragmented file, and why the rewrite
//!
//! The muxer writes fragments every two seconds while recording, so a crash
//! leaves a playable file up to the last fragment. Upstream's AVAssetWriter
//! then finalises the movie on a normal stop: the file it leaves behind is a
//! regular `ftyp ... mdat moov`, because the fragments are indexed by a moov
//! that is appended at the end — no media bytes move.
//!
//! GStreamer's `qtmux`/`mp4mux` cannot do that second step, so this module does
//! it: it parses the init `moov` and every `moof`, builds the sample tables
//! (`stts`, `ctts`, `stss`, `stsz`, `stsc`, `stco`/`co64`) from the fragment
//! headers, appends a regular `moov` at the end of the file, and only then
//! flips the old init `moov` and all `moof` headers to `free` in place.
//!
//! # Crash safety of the finalise itself
//!
//! - Crash before the append: the fragmented file is untouched and playable.
//! - Crash after the append, before the flip: two moovs. Players read the first
//!   (the fragmented one) and skip the duplicate, so the file is still playable.
//! - Crash during the flip: the appended regular moov is complete, the old moov
//!   and some `moof` boxes are `free`. The leftover `moof` boxes are unknown
//!   boxes at the top level and are ignored; the regular moov plays.
//!
//! Finalisation never deletes or truncates data. Explicit recovery can discard
//! an incomplete tail, or copy the complete prefix to a new rescue file.
//!
//! # Track layout
//!
//! `qtmux` writes every track with `tkhd` flags `0x7` (enabled, in movie, in
//! preview) and no alternate group, and it cannot write an edit list that skips
//! an AAC encoder's priming. Both are patched here while the moov is rebuilt:
//! audio tracks can share an alternate group with only the default track
//! enabled, and each audio track gets an `elst` whose `media_time` is its
//! priming, so players start at the first real sample (what AVFoundation's
//! edit list does upstream).
//!
//! Written for this port from the ISO/IEC 14496-12 box layout as qtmux emits
//! it, and verified against qtmux output in the tests.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};

/// A box found in a file or buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BoxRef {
    kind: [u8; 4],
    /// Offset of the box header, relative to the buffer or file.
    start: u64,
    /// Whole box size, header included.
    size: u64,
    /// Header size: 8, or 16 with a 64-bit size.
    header: u64,
}

impl BoxRef {
    #[allow(dead_code)] // used by the tests' fixture inspection
    fn body_start(&self) -> u64 {
        self.start + self.header
    }
}

const FREE: [u8; 4] = *b"free";
const MOOV: [u8; 4] = *b"moov";
const MOOF: [u8; 4] = *b"moof";

/// What a track carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handler {
    Video,
    Audio,
    Other([u8; 4]),
}

impl Handler {
    fn parse(kind: [u8; 4]) -> Handler {
        match &kind {
            b"vide" => Handler::Video,
            b"soun" => Handler::Audio,
            _ => Handler::Other(kind),
        }
    }

    /// One word for `probe`.
    pub fn label(self) -> String {
        match self {
            Handler::Video => "video".into(),
            Handler::Audio => "audio".into(),
            Handler::Other(k) => String::from_utf8_lossy(&k).into_owned(),
        }
    }
}

/// One edit-list entry: skip `media_time` units of media, show
/// `segment_duration` movie units of it. `media_time` −1 is an empty edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    pub segment_duration: u64,
    pub media_time: i64,
}

/// What `probe` prints about one track.
#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub track_id: u32,
    pub handler: Handler,
    /// Media timescale (units per second).
    pub timescale: u32,
    /// Media duration in `timescale` units.
    pub duration: u64,
    /// `tkhd` track_enabled flag.
    pub enabled: bool,
    /// `tkhd` alternate_group.
    pub alternate_group: u16,
    pub edits: Vec<Edit>,
    /// Samples in the movie (from the fragment headers or `stsz`).
    pub samples: u64,
    /// Total bytes of those samples, for the data rate `probe` prints.
    pub bytes: u64,
    /// Presentation start in seconds (the first fragment's decode time; 0 for
    /// a regular movie).
    pub first_pts: f64,
    /// The track's nominal frame rate (the most common sample duration), not
    /// the effective one: a file with dropped slots still reads 60.000 fps.
    pub nominal_fps: Option<f64>,
}

/// The movie as `probe` reports it.
#[derive(Clone, Debug)]
pub struct MovieInfo {
    pub movie_timescale: u32,
    pub duration: u64,
    pub fragmented: bool,
    pub tracks: Vec<TrackInfo>,
}

/// The track layout the writer wants in the final file.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    /// Give every audio track this `tkhd` alternate group.
    pub audio_alternate_group: Option<u16>,
    /// Index among the audio tracks (in file order) that stays enabled. The
    /// others are disabled alternatives.
    pub default_audio: Option<usize>,
    /// AAC priming in samples at 48 kHz for each audio track, in file order.
    /// Each becomes its `elst` `media_time` (scaled to the track's timescale).
    pub audio_priming: Vec<u64>,
}

/// Kill the process at a chosen point of the finalise, to prove the file on
/// disk is playable in the interrupted state (`REC_DEFRAG_CRASH`, tests only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CrashAt {
    #[default]
    None,
    /// After the regular moov is appended and flushed, before any box is
    /// flipped: the file holds two moovs.
    BeforeFlip,
    /// In the middle of flipping the boxes to `free`.
    MidFlip,
}

/// What the finalise did, for the log and the tests.
#[derive(Clone, Debug, Default)]
pub struct FinalizeReport {
    /// The movie was fragmented and got a regular moov built for it.
    pub defragmented: bool,
    pub fragments: usize,
    pub tracks: usize,
    pub samples: u64,
    pub input_bytes: u64,
    pub moov_bytes: usize,
    pub millis: u128,
}

// ---------------------------------------------------------------------------
// Reading boxes
// ---------------------------------------------------------------------------

fn be32(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| anyhow!("truncated box field at {at}"))
}

fn be16(b: &[u8], at: usize) -> Result<u16> {
    b.get(at..at + 2)
        .map(|s| u16::from_be_bytes([s[0], s[1]]))
        .ok_or_else(|| anyhow!("truncated box field at {at}"))
}

fn be64(b: &[u8], at: usize) -> Result<u64> {
    b.get(at..at + 8)
        .map(|s| u64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
        .ok_or_else(|| anyhow!("truncated box field at {at}"))
}

/// Boxes in `buf[start..end]`, offsets relative to `buf`.
fn children(buf: &[u8], start: usize, end: usize) -> Result<Vec<BoxRef>> {
    let mut out = vec![];
    let mut at = start;
    while at + 8 <= end {
        let size32 = be32(buf, at)? as u64;
        let kind: [u8; 4] = buf[at + 4..at + 8].try_into().unwrap();
        let (size, header) = match size32 {
            0 => ((end - at) as u64, 8),
            1 => (be64(buf, at + 8)?, 16),
            s => (s, 8),
        };
        if size < header || at as u64 + size > end as u64 {
            bail!("bad box size {size} at {at}");
        }
        out.push(BoxRef {
            kind,
            start: at as u64,
            size,
            header,
        });
        at += size as usize;
    }
    Ok(out)
}

fn find<'a>(boxes: &'a [BoxRef], kind: &[u8; 4]) -> Option<&'a BoxRef> {
    boxes.iter().find(|b| &b.kind == kind)
}

fn slice<'a>(buf: &'a [u8], b: &BoxRef) -> &'a [u8] {
    &buf[b.start as usize..(b.start + b.size) as usize]
}

/// Scan top-level boxes without reading their payloads.
fn scan(path: &Path) -> Result<(File, Vec<BoxRef>, u64)> {
    scan_with(path, false)
}

/// `scan`, optionally tolerating a truncated last box.
///
/// A SIGKILL can land in the middle of writing an `mdat`, so a crashed
/// recording's last box is short. `probe` must still report the file (it plays
/// up to the last complete fragment); the finalise refuses such a file.
fn scan_with(path: &Path, tolerant: bool) -> Result<(File, Vec<BoxRef>, u64)> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len();
    let mut boxes = vec![];
    let mut at = 0u64;
    while at + 8 <= len {
        file.seek(SeekFrom::Start(at))?;
        let mut hdr = [0u8; 16];
        file.read_exact(&mut hdr[..8])?;
        let size32 = u32::from_be_bytes(hdr[0..4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = hdr[4..8].try_into().unwrap();
        let (size, header) = match size32 {
            0 => (len - at, 8),
            1 => {
                file.read_exact(&mut hdr[8..16])?;
                (u64::from_be_bytes(hdr[8..16].try_into().unwrap()), 16)
            }
            s => (s, 8),
        };
        if size < header || at + size > len {
            if tolerant && at + header <= len {
                break; // a truncated tail from a crash
            }
            bail!(
                "{}: bad box {} at {at} (size {size}, file {len})",
                path.display(),
                String::from_utf8_lossy(&kind)
            );
        }
        boxes.push(BoxRef {
            kind,
            start: at,
            size,
            header,
        });
        at += size;
    }
    Ok((file, boxes, len))
}

/// Complete recoverable prefix of an open fragmented recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    pub input_bytes: u64,
    /// Includes complete trailing index/free boxes, but never an orphan moof.
    pub complete_bytes: u64,
    pub last_fragment_end: u64,
    pub fragments: usize,
}

/// Inspect the existing descriptor, including a torn ordinary or extended header.
/// The caller must stop the writer first. File position is preserved; I/O errors
/// propagate rather than being mistaken for a torn tail. Size-zero boxes cannot
/// prove that a write completed, so recovery stops before them.
pub fn inspect_recovery(file: &File) -> Result<RecoveryReport> {
    use std::os::unix::fs::FileExt;
    let len = file.metadata()?.len();
    let mut report = RecoveryReport {
        input_bytes: len,
        complete_bytes: 0,
        last_fragment_end: 0,
        fragments: 0,
    };
    let mut at = 0;
    let mut pending = false;
    let mut init = false;
    while len - at >= 8 {
        let mut hdr = [0; 16];
        file.read_exact_at(&mut hdr[..8], at)?;
        let small = u32::from_be_bytes(hdr[..4].try_into().unwrap());
        let kind: [u8; 4] = hdr[4..8].try_into().unwrap();
        let (size, header) = match small {
            0 => break,
            1 if len - at < 16 => break,
            1 => {
                file.read_exact_at(&mut hdr[8..], at + 8)?;
                (u64::from_be_bytes(hdr[8..].try_into().unwrap()), 16)
            }
            n => (u64::from(n), 8),
        };
        if size < header || size > len - at {
            break;
        }
        if kind == *b"moof" {
            if pending || !init {
                break;
            }
            pending = true;
        } else if kind == *b"mdat" {
            if !pending {
                break;
            }
            pending = false;
            report.fragments += 1;
            report.last_fragment_end = at + size;
        } else if pending {
            break;
        } else if kind == *b"moov" {
            init = true;
        }
        at += size;
        if !pending {
            report.complete_bytes = at;
        }
    }
    if report.fragments == 0 {
        bail!("recording has no complete moof/mdat fragments to recover");
    }
    Ok(report)
}

/// Discard only the incomplete suffix, without appending a regular index (so
/// ENOSPC recovery needs no additional allocation). No change if already whole.
pub fn truncate_recovery(file: &File) -> Result<RecoveryReport> {
    let report = inspect_recovery(file)?;
    if report.complete_bytes < report.input_bytes {
        file.set_len(report.complete_bytes)?;
        file.sync_all()?;
    }
    Ok(report)
}

/// Rescue from the still-open source descriptor, even if its path disappeared.
/// Never overwrites an existing destination. The source and its cursor are not
/// modified. A failed copy may leave a partial destination for diagnosis.
pub fn copy_recovery(file: &File, destination: &Path) -> Result<RecoveryReport> {
    use std::os::unix::fs::FileExt;
    let report = inspect_recovery(file)?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut buffer = vec![0; 1024 * 1024];
    let mut at = 0;
    while at < report.complete_bytes {
        let count = (report.complete_bytes - at).min(buffer.len() as u64) as usize;
        file.read_exact_at(&mut buffer[..count], at)?;
        out.write_all(&buffer[..count])?;
        at += count as u64;
    }
    out.sync_all()?;
    Ok(report)
}

fn read_at(file: &mut File, offset: u64, size: u64) -> Result<Vec<u8>> {
    if size > 128 * 1024 * 1024 {
        bail!("refusing to read a {size} byte box into memory");
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut out = vec![0u8; size as usize];
    file.read_exact(&mut out)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Parsing the movie
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
struct Trex {
    duration: u32,
    size: u32,
    flags: u32,
}

struct InitTrack {
    info: TrackInfo,
}

struct Init {
    movie_timescale: u32,
    duration: u64,
    tracks: Vec<InitTrack>,
    has_mvex: bool,
    /// Defaults for sample fields a `trun`/`tfhd` leaves out.
    trex: HashMap<u32, Trex>,
}

fn parse_tkhd(b: &[u8]) -> Result<(u32, u32, u16, u64)> {
    if b.len() < 92 || &b[4..8] != b"tkhd" {
        bail!("not a tkhd box");
    }
    let version = b[8];
    let flags = ((b[9] as u32) << 16) | ((b[10] as u32) << 8) | b[11] as u32;
    let (id_at, dur_at, wide) = if version == 1 {
        (28, 36, true)
    } else {
        (20, 28, false)
    };
    let track_id = be32(b, id_at)?;
    let duration = if wide {
        be64(b, dur_at)?
    } else {
        be32(b, dur_at)? as u64
    };
    let alt_at = if version == 1 { 54 } else { 42 };
    Ok((track_id, flags, be16(b, alt_at)?, duration))
}

fn parse_mdhd(b: &[u8]) -> Result<(u32, u64)> {
    if b.len() < 32 || &b[4..8] != b"mdhd" {
        bail!("not an mdhd box");
    }
    if b[8] == 1 {
        Ok((be32(b, 28)?, be64(b, 32)?))
    } else {
        Ok((be32(b, 20)?, be32(b, 24)? as u64))
    }
}

fn parse_elst(b: &[u8]) -> Result<Vec<Edit>> {
    if b.len() < 16 || &b[4..8] != b"elst" {
        bail!("not an elst box");
    }
    let version = b[8];
    let count = be32(b, 12)? as usize;
    let mut out = Vec::with_capacity(count);
    let mut at = 16;
    for _ in 0..count {
        if version == 1 {
            out.push(Edit {
                segment_duration: be64(b, at)?,
                media_time: be64(b, at + 8)? as i64,
            });
            at += 20;
        } else {
            out.push(Edit {
                segment_duration: be32(b, at)? as u64,
                media_time: be32(b, at + 4)? as i32 as i64,
            });
            at += 12;
        }
    }
    Ok(out)
}

/// `stsz`: (fixed size or 0, sample count, total bytes).
fn parse_stsz(b: &[u8]) -> Result<(u32, u32, u64)> {
    if b.len() < 20 || &b[4..8] != b"stsz" {
        bail!("not an stsz box");
    }
    let fixed = be32(b, 12)?;
    let count = be32(b, 16)?;
    if fixed > 0 {
        return Ok((fixed, count, fixed as u64 * count as u64));
    }
    let mut bytes = 0u64;
    for i in 0..count as usize {
        bytes += be32(b, 20 + i * 4)? as u64;
    }
    Ok((0, count, bytes))
}

/// The most common sample duration in an `stts` table (the nominal frame
/// interval), or None for an empty table.
fn parse_stts_mode(b: &[u8]) -> Result<Option<u32>> {
    if b.len() < 16 || &b[4..8] != b"stts" {
        bail!("not an stts box");
    }
    let n = be32(b, 12)? as usize;
    let mut best: Option<(u32, u32)> = None;
    let mut at = 16;
    for _ in 0..n {
        let count = be32(b, at)?;
        let delta = be32(b, at + 4)?;
        at += 8;
        if best.is_none_or(|(bc, bd)| count > bc || (count == bc && delta < bd)) {
            best = Some((count, delta));
        }
    }
    Ok(best.map(|(_, d)| d))
}

fn parse_moov(moov: &[u8]) -> Result<Init> {
    if moov.len() < 8 || &moov[4..8] != b"moov" {
        bail!("not a moov box");
    }
    let kids = children(moov, 8, moov.len())?;
    let mvhd_ref = find(&kids, b"mvhd").context("moov has no mvhd")?;
    let mv = slice(moov, mvhd_ref);
    let (movie_timescale, duration) = if mv[8] == 1 {
        (be32(mv, 28)?, be64(mv, 32)?)
    } else {
        (be32(mv, 20)?, be32(mv, 24)? as u64)
    };
    if movie_timescale == 0 {
        bail!("movie timescale is zero");
    }
    let mut tracks = vec![];
    for trak_ref in kids.iter().filter(|b| &b.kind == b"trak") {
        let trak = slice(moov, trak_ref).to_vec();
        let tk = children(&trak, 8, trak.len())?;
        let tkhd_ref = find(&tk, b"tkhd").context("trak has no tkhd")?;
        let (track_id, flags, alternate_group, tkhd_duration) = parse_tkhd(slice(&trak, tkhd_ref))?;
        let mut handler = Handler::Other(*b"    ");
        let mut timescale = 0u32;
        let mut duration = tkhd_duration;
        let mut samples = 0u64;
        let mut bytes = 0u64;
        let mut nominal_fps = None;
        let mut edits = vec![];
        if let Some(mdia_ref) = find(&tk, b"mdia") {
            let mdia = slice(&trak, mdia_ref);
            let mk = children(mdia, 8, mdia.len())?;
            if let Some(m) = find(&mk, b"mdhd") {
                let (ts, d) = parse_mdhd(slice(mdia, m))?;
                timescale = ts;
                duration = d;
            }
            if let Some(h) = find(&mk, b"hdlr") {
                let hb = slice(mdia, h);
                handler = Handler::parse(hb[16..20].try_into().unwrap());
            }
            if let Some(minf_ref) = find(&mk, b"minf") {
                let minf = slice(mdia, minf_ref);
                let mk2 = children(minf, 8, minf.len())?;
                if let Some(stbl_ref) = find(&mk2, b"stbl") {
                    let stbl = slice(minf, stbl_ref);
                    let sk = children(stbl, 8, stbl.len())?;
                    if let Some(sz) = find(&sk, b"stsz") {
                        let (_, count, total) = parse_stsz(slice(stbl, sz))?;
                        samples = count as u64;
                        bytes = total;
                    }
                    if let Some(table) = find(&sk, b"stts") {
                        nominal_fps = parse_stts_mode(slice(stbl, table))?
                            .filter(|d| *d > 0)
                            .map(|d| timescale as f64 / d as f64);
                    }
                }
            }
        }
        if let Some(edts_ref) = find(&tk, b"edts") {
            let e = slice(&trak, edts_ref);
            let ek = children(e, 8, e.len())?;
            if let Some(el) = find(&ek, b"elst") {
                edits = parse_elst(slice(e, el))?;
            }
        }
        tracks.push(InitTrack {
            info: TrackInfo {
                track_id,
                handler,
                timescale,
                duration,
                enabled: flags & 1 != 0,
                alternate_group,
                edits,
                samples,
                bytes,
                first_pts: 0.0,
                nominal_fps,
            },
        });
    }
    let mut has_mvex = false;
    let mut trex = HashMap::new();
    if let Some(mvex_ref) = find(&kids, b"mvex") {
        has_mvex = true;
        let m = slice(moov, mvex_ref);
        for t in children(m, 8, m.len())?
            .iter()
            .filter(|b| &b.kind == b"trex")
        {
            let tb = slice(m, t);
            trex.insert(
                be32(tb, 12)?,
                Trex {
                    duration: be32(tb, 20)?,
                    size: be32(tb, 24)?,
                    flags: be32(tb, 28)?,
                },
            );
        }
    }
    Ok(Init {
        movie_timescale,
        duration,
        tracks,
        has_mvex,
        trex,
    })
}

/// Read a recording's movie structure: `probe` prints it.
pub fn read(path: &Path) -> Result<MovieInfo> {
    let (mut file, boxes, _len) = scan_with(path, true)?;
    // After a finalise a file has two moovs for a moment; the last is the
    // regular one. Before it, the only moov is the fragmented init.
    let moov = boxes
        .iter()
        .rfind(|b| b.kind == MOOV)
        .context("no moov box in the movie")?;
    let buf = read_at(&mut file, moov.start, moov.size)?;
    let init = parse_moov(&buf)?;
    // Only the fragmented init moov describes fragments. A regular moov that
    // happens to sit in a file with leftover `moof` boxes (the state after the
    // append, or after a crash in the flip) already has every sample in its
    // tables; reading the moofs as well would count them twice.
    let fragmented = init.has_mvex;
    let mut tracks: Vec<TrackInfo> = init.tracks.into_iter().map(|t| t.info).collect();
    if fragmented {
        // The init moov has no sample tables; take the counts, bytes, duration
        // and presentation start from the fragment headers so `probe` reports
        // the real movie (a crash-left file is as long as its samples).
        let mut per_track: HashMap<u32, Vec<Sample>> = HashMap::new();
        let mut firsts: HashMap<u32, FragmentClock> = HashMap::new();
        let mut ends: HashMap<u32, u64> = HashMap::new();
        for b in boxes.iter().filter(|b| b.kind == MOOF) {
            let buf = read_at(&mut file, b.start, b.size)?;
            for (tid, decode_time, samples) in parse_moof(&buf, b.start, &init.trex)? {
                if samples.is_empty() {
                    continue;
                }
                // Probe describes the fragmented timeline, not a proposed stts
                // rewrite. External muxers (e.g. MediaMTX) can emit overlapping
                // decode ranges: count every packet but measure the timeline's
                // extent instead of rejecting it or summing overlapping time.
                // Keep the preceding fragment's end separately from the maximum
                // end, since an absent tfdt continues that preceding fragment.
                let clock = firsts.entry(tid).or_insert_with(|| {
                    let first = decode_time.unwrap_or(0);
                    FragmentClock { first, end: first }
                });
                let start = decode_time.unwrap_or(clock.end);
                clock.first = clock.first.min(start);
                clock.end = start
                    .checked_add(media_duration(&samples))
                    .context("track decode time overflow")?;
                ends.entry(tid)
                    .and_modify(|end| *end = (*end).max(clock.end))
                    .or_insert(clock.end);
                per_track.entry(tid).or_default().extend(samples);
            }
        }
        for t in tracks.iter_mut() {
            let Some(samples) = per_track.get(&t.track_id) else {
                continue;
            };
            let samples = if t.handler == Handler::Audio {
                sane_durations(samples, t.timescale)
            } else {
                samples.to_vec()
            };
            t.samples = samples.len() as u64;
            t.bytes = samples.iter().map(|s| s.size as u64).sum();
            t.duration = if t.handler == Handler::Video {
                ends[&t.track_id] - firsts[&t.track_id].first
            } else {
                media_duration(&samples)
            };
            t.first_pts = firsts.get(&t.track_id).map_or(0, |clock| clock.first) as f64
                / t.timescale.max(1) as f64;
            let mut durations: HashMap<u32, u32> = HashMap::new();
            for s in &samples {
                *durations.entry(s.duration).or_default() += 1;
            }
            let mode = durations
                .iter()
                .max_by_key(|(d, n)| (**n, std::cmp::Reverse(**d)))
                .map(|(d, _)| *d)
                .filter(|d| *d > 0);
            t.nominal_fps = mode.map(|d| t.timescale as f64 / d as f64);
        }
    }
    Ok(MovieInfo {
        movie_timescale: init.movie_timescale,
        duration: init.duration,
        fragmented,
        tracks,
    })
}

// ---------------------------------------------------------------------------
// Parsing a moof
// ---------------------------------------------------------------------------

/// One sample of a fragment: where its bytes are and how long it lasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sample {
    /// Absolute file offset of the sample's first byte.
    offset: u64,
    size: u32,
    duration: u32,
    /// Composition offset (media timescale units).
    cts: i32,
    /// False for a non-sync sample (`sample_is_non_sync_sample`).
    sync: bool,
}

/// Track id, optional base decode time, and samples of a single `traf`.
type TrackFragment = (u32, Option<u64>, Vec<Sample>);

/// Samples of every track in one `moof`, in decode order.
///
/// `moof_offset` is the box's absolute offset in the file: the base for a
/// `trun`'s `data_offset` when its `tfhd` has no `base_data_offset` (what
/// qtmux writes).
fn parse_moof(
    buf: &[u8],
    moof_offset: u64,
    trex: &HashMap<u32, Trex>,
) -> Result<Vec<TrackFragment>> {
    if buf.len() < 8 || &buf[4..8] != b"moof" {
        bail!("not a moof box");
    }
    let kids = children(buf, 8, buf.len())?;
    let mut out = vec![];
    // Base offset for a traf without `base_data_offset`: the end of the
    // previous traf's data (ISO 14496-12 §8.8.7).
    let mut running_base = moof_offset;
    for traf_ref in kids.iter().filter(|b| &b.kind == b"traf") {
        let traf = slice(buf, traf_ref);
        let tk = children(traf, 8, traf.len())?;
        let tfhd_ref = find(&tk, b"tfhd").context("traf has no tfhd")?;
        let tfhd = slice(traf, tfhd_ref);
        let tflags = ((tfhd[9] as u32) << 16) | ((tfhd[10] as u32) << 8) | tfhd[11] as u32;
        let track_id = be32(tfhd, 12)?;
        let defaults = trex.get(&track_id).copied().unwrap_or_default();
        let mut at = 16usize;
        let base = if tflags & 0x1 != 0 {
            let b = be64(tfhd, at)?;
            at += 8;
            b
        } else {
            running_base
        };
        let mut default_size = None;
        let mut default_duration = None;
        let mut default_flags = None;
        if tflags & 0x2 != 0 {
            at += 4; // sample_description_index
        }
        if tflags & 0x8 != 0 {
            default_duration = Some(be32(tfhd, at)?);
            at += 4;
        }
        if tflags & 0x10 != 0 {
            default_size = Some(be32(tfhd, at)?);
            at += 4;
        }
        if tflags & 0x20 != 0 {
            default_flags = Some(be32(tfhd, at)?);
        }
        // The fragment's decode time (the track's presentation start in a
        // fragmented movie).
        let decode_time = if let Some(dt) = find(&tk, b"tfdt") {
            let b = slice(traf, dt);
            Some(if b[8] == 1 {
                be64(b, 12)?
            } else {
                be32(b, 12)? as u64
            })
        } else {
            None
        };
        let mut samples: Vec<Sample> = vec![];
        let mut data_end = base;
        for trun_ref in tk.iter().filter(|b| &b.kind == b"trun") {
            let trun = slice(traf, trun_ref);
            let version = trun[8];
            let rflags = ((trun[9] as u32) << 16) | ((trun[10] as u32) << 8) | trun[11] as u32;
            let count = be32(trun, 12)? as usize;
            let mut p = 16usize;
            let mut offset = if rflags & 0x1 != 0 {
                let d = be32(trun, p)? as i32 as i64;
                p += 4;
                (base as i64 + d).max(0) as u64
            } else {
                data_end
            };
            let first_flags = if rflags & 0x4 != 0 {
                let f = be32(trun, p)?;
                p += 4;
                Some(f)
            } else {
                None
            };
            for i in 0..count {
                let duration = if rflags & 0x100 != 0 {
                    let v = be32(trun, p)?;
                    p += 4;
                    v
                } else {
                    default_duration.unwrap_or(defaults.duration)
                };
                let size = if rflags & 0x200 != 0 {
                    let v = be32(trun, p)?;
                    p += 4;
                    v
                } else {
                    default_size.unwrap_or(defaults.size)
                };
                let flags = if rflags & 0x400 != 0 {
                    let v = be32(trun, p)?;
                    p += 4;
                    v
                } else if i == 0 && first_flags.is_some() {
                    first_flags.unwrap()
                } else {
                    default_flags.unwrap_or(defaults.flags)
                };
                let cts = if rflags & 0x800 != 0 {
                    let v = be32(trun, p)?;
                    p += 4;
                    // qtmux writes a *negative* composition offset (the
                    // compensation for a retimed audio duration) as two's
                    // complement even in a version-0 trun, whose offsets the
                    // spec calls unsigned. Reading it as unsigned saturated it
                    // to i32::MAX and put the 11th audio packet at 44739 s.
                    let _ = version;
                    v as i32
                } else {
                    0
                };
                samples.push(Sample {
                    offset,
                    size,
                    duration,
                    cts,
                    sync: flags & 0x0001_0000 == 0,
                });
                offset += size as u64;
            }
            data_end = offset;
        }
        running_base = data_end;
        // Keep each traf's boundary, even when a moof contains multiple
        // trafs for the same track: each can have its own tfdt.
        out.push((track_id, decode_time, samples));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Building the sample tables
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Chunk {
    offset: u64,
    count: u32,
}

/// `stbl` sample tables for one track.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Tables {
    stts: Vec<(u32, u32)>,
    /// None when no composition offset is needed.
    ctts: Option<(u8, Vec<(u32, i32)>)>,
    /// Empty means every sample is a sync sample (no `stss`).
    stss: Vec<u32>,
    /// The common size when every sample has it, else None and `sizes` is used.
    fixed_size: Option<u32>,
    sizes: Vec<u32>,
    /// `stsc` runs: (first_chunk, samples_per_chunk), sample description 1.
    stsc: Vec<(u32, u32)>,
    chunks: Vec<Chunk>,
    /// True when an offset does not fit 32 bits (`co64`).
    co64: bool,
}

#[derive(Clone, Copy)]
struct FragmentClock {
    first: u64,
    end: u64,
}

/// Append a fragment without losing video decode-time gaps at its boundary.
///
/// Sample tables start at zero, relative to the first fragment's decode time.
/// A later tfdt can leave a hole not represented in the preceding trun: hold
/// its last frame across that hole rather than shifting every following PTS.
/// Missing tfdt means continuation, not an explicit reset to zero. Audio is
/// deliberately left to its separate qtmux duration repair. This is only for
/// finalization: unlike a probe, regular sample tables cannot represent
/// overlapping fragment decode ranges without changing packet timestamps.
fn append_fragment(
    timeline: &mut Vec<Sample>,
    firsts: &mut HashMap<u32, FragmentClock>,
    track_id: u32,
    decode_time: Option<u64>,
    samples: Vec<Sample>,
    video: bool,
) -> Result<()> {
    if samples.is_empty() {
        return Ok(());
    }
    let clock = firsts.entry(track_id).or_insert_with(|| {
        let first = decode_time.unwrap_or(0);
        FragmentClock { first, end: first }
    });
    if video
        && !timeline.is_empty()
        && let Some(time) = decode_time
    {
        let expected = clock.end;
        if time < expected {
            bail!("video track {track_id} has overlapping fragment decode times");
        }
        let last = timeline.last_mut().unwrap();
        last.duration = u32::try_from(last.duration as u64 + time - expected)
            .context("video fragment gap exceeds sample duration range")?;
        clock.end = time;
    }
    clock.end = clock
        .end
        .checked_add(media_duration(&samples))
        .context("track decode time overflow")?;
    timeline.extend(samples);
    Ok(())
}

/// Repair an audio track's sample durations.
///
/// qtmux re-times audio buffers whose duration it considers wrong
/// (`max-raw-audio-drift`): a real PipeWire mic delivers 1024/2048-frame
/// buffers, and the truns then contain duration 0 or a negative duration
/// written as a huge `u32` (the demuxer's fragmented timeline even jumps back
/// inside a fragment). Copying those into a regular `stts` produced a track
/// whose 11th packet was at 44739 s, so a finalised mic track decoded 0.2 s of
/// a 4 s recording. When any duration is implausible, rebuild a uniform
/// timeline from the most common plausible one.
fn sane_durations(samples: &[Sample], timescale: u32) -> Vec<Sample> {
    let mut counts: HashMap<u32, u32> = HashMap::new();
    for s in samples
        .iter()
        .filter(|s| s.duration > 0 && s.duration <= timescale)
    {
        *counts.entry(s.duration).or_default() += 1;
    }
    let mode = counts
        .iter()
        .max_by_key(|(d, n)| (**n, std::cmp::Reverse(**d)))
        .map(|(d, _)| *d)
        .unwrap_or(0);
    if mode == 0 {
        return samples.to_vec();
    }
    let irregular = samples
        .iter()
        .any(|s| s.duration == 0 || s.duration > timescale);
    if !irregular {
        return samples.to_vec();
    }
    // The composition offsets were compensating the retimed durations; with a
    // uniform decode timeline they would double-count, so drop them.
    samples
        .iter()
        .map(|s| Sample {
            duration: mode,
            cts: 0,
            ..*s
        })
        .collect()
}

fn build_tables(samples: &[Sample]) -> Tables {
    let mut t = Tables::default();
    if samples.is_empty() {
        return t;
    }
    // A chunk is a run of contiguous samples (one trun, in qtmux's output).
    let mut chunks: Vec<Chunk> = vec![];
    for (i, s) in samples.iter().enumerate() {
        if i == 0 || s.offset != samples[i - 1].offset + samples[i - 1].size as u64 {
            chunks.push(Chunk {
                offset: s.offset,
                count: 0,
            });
        }
        chunks.last_mut().unwrap().count += 1;
    }
    let mut last_count = 0u32;
    for (i, c) in chunks.iter().enumerate() {
        if c.count != last_count {
            t.stsc.push(((i + 1) as u32, c.count));
            last_count = c.count;
        }
    }
    t.co64 = chunks.iter().any(|c| c.offset > u32::MAX as u64);
    t.chunks = chunks;

    for s in samples {
        match t.stts.last_mut() {
            Some((n, d)) if *d == s.duration => *n += 1,
            _ => t.stts.push((1, s.duration)),
        }
    }
    if samples.iter().all(|s| s.size == samples[0].size) {
        t.fixed_size = Some(samples[0].size);
    } else {
        t.sizes = samples.iter().map(|s| s.size).collect();
    }
    if samples.iter().any(|s| !s.sync) {
        t.stss = samples
            .iter()
            .enumerate()
            .filter(|(_, s)| s.sync)
            .map(|(i, _)| (i + 1) as u32)
            .collect();
    }
    if samples.iter().any(|s| s.cts != 0) {
        let version = u8::from(samples.iter().any(|s| s.cts < 0));
        let mut runs: Vec<(u32, i32)> = vec![];
        for s in samples {
            match runs.last_mut() {
                Some((n, o)) if *o == s.cts => *n += 1,
                _ => runs.push((1, s.cts)),
            }
        }
        t.ctts = Some((version, runs));
    }
    t
}

fn media_duration(samples: &[Sample]) -> u64 {
    samples.iter().map(|s| s.duration as u64).sum()
}

/// Give the last sample the remaining duration up to the take's end.
///
/// A slot dropped at the end of the take (the encoder was busy, or the drain
/// at stop refused it) leaves the last written sample no further, so the video
/// track ended short of the audio and the A/V offset grew with every trailing
/// drop. Mid-take gaps are already in the durations and are left alone; only
/// the last sample is extended.
fn extend_last_sample(samples: &mut [Sample], end: u64) {
    let start: u64 = samples
        .iter()
        .take(samples.len().saturating_sub(1))
        .map(|s| s.duration as u64)
        .sum();
    if let Some(last) = samples.last_mut()
        && end > start + last.duration as u64
    {
        last.duration = (end - start).min(u32::MAX as u64) as u32;
    }
}

// ---------------------------------------------------------------------------
// Writing boxes
// ---------------------------------------------------------------------------

fn push_box(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
}

fn push_full_box(out: &mut Vec<u8>, kind: &[u8; 4], version: u8, body: &[u8]) {
    let mut b = Vec::with_capacity(body.len() + 4);
    b.push(version);
    b.extend_from_slice(&[0, 0, 0]);
    b.extend_from_slice(body);
    push_box(out, kind, &b);
}

fn stbl_table_boxes(t: &Tables, count: u32) -> Vec<u8> {
    let mut out = vec![];
    // stts
    {
        let mut body = Vec::with_capacity(4 + t.stts.len() * 8);
        body.extend_from_slice(&(t.stts.len() as u32).to_be_bytes());
        for (n, d) in &t.stts {
            body.extend_from_slice(&n.to_be_bytes());
            body.extend_from_slice(&d.to_be_bytes());
        }
        push_full_box(&mut out, b"stts", 0, &body);
    }
    // ctts
    if let Some((version, runs)) = &t.ctts {
        let mut body = Vec::with_capacity(4 + runs.len() * 8);
        body.extend_from_slice(&(runs.len() as u32).to_be_bytes());
        for (n, o) in runs {
            body.extend_from_slice(&n.to_be_bytes());
            body.extend_from_slice(&o.to_be_bytes());
        }
        push_full_box(&mut out, b"ctts", *version, &body);
    }
    // stss
    if !t.stss.is_empty() {
        let mut body = Vec::with_capacity(4 + t.stss.len() * 4);
        body.extend_from_slice(&(t.stss.len() as u32).to_be_bytes());
        for n in &t.stss {
            body.extend_from_slice(&n.to_be_bytes());
        }
        push_full_box(&mut out, b"stss", 0, &body);
    }
    // stsz
    {
        let mut body = Vec::with_capacity(8 + t.sizes.len() * 4);
        match t.fixed_size {
            Some(size) => {
                body.extend_from_slice(&size.to_be_bytes());
                body.extend_from_slice(&count.to_be_bytes());
            }
            None => {
                body.extend_from_slice(&0u32.to_be_bytes());
                body.extend_from_slice(&(t.sizes.len() as u32).to_be_bytes());
                for s in &t.sizes {
                    body.extend_from_slice(&s.to_be_bytes());
                }
            }
        }
        push_full_box(&mut out, b"stsz", 0, &body);
    }
    // stsc
    {
        let mut body = Vec::with_capacity(4 + t.stsc.len() * 12);
        body.extend_from_slice(&(t.stsc.len() as u32).to_be_bytes());
        for (first, per) in &t.stsc {
            body.extend_from_slice(&first.to_be_bytes());
            body.extend_from_slice(&per.to_be_bytes());
            body.extend_from_slice(&1u32.to_be_bytes());
        }
        push_full_box(&mut out, b"stsc", 0, &body);
    }
    // stco / co64
    if t.co64 {
        let mut body = Vec::with_capacity(4 + t.chunks.len() * 8);
        body.extend_from_slice(&(t.chunks.len() as u32).to_be_bytes());
        for c in &t.chunks {
            body.extend_from_slice(&c.offset.to_be_bytes());
        }
        push_full_box(&mut out, b"co64", 0, &body);
    } else {
        let mut body = Vec::with_capacity(4 + t.chunks.len() * 4);
        body.extend_from_slice(&(t.chunks.len() as u32).to_be_bytes());
        for c in &t.chunks {
            body.extend_from_slice(&(c.offset as u32).to_be_bytes());
        }
        push_full_box(&mut out, b"stco", 0, &body);
    }
    out
}

fn edts_box(edits: &[Edit]) -> Vec<u8> {
    let large = edits.iter().any(|e| {
        e.segment_duration > u32::MAX as u64 || e.media_time > i32::MAX as i64 || e.media_time < -1
    });
    let mut body = Vec::with_capacity(4 + edits.len() * 20);
    body.extend_from_slice(&(edits.len() as u32).to_be_bytes());
    for e in edits {
        if large {
            body.extend_from_slice(&e.segment_duration.to_be_bytes());
            body.extend_from_slice(&e.media_time.to_be_bytes());
        } else {
            body.extend_from_slice(&(e.segment_duration as u32).to_be_bytes());
            body.extend_from_slice(&(e.media_time as i32).to_be_bytes());
        }
        body.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // 1.0
    }
    let mut elst = vec![];
    push_full_box(&mut elst, b"elst", u8::from(large), &body);
    let mut out = vec![];
    push_box(&mut out, b"edts", &elst);
    out
}

// ---------------------------------------------------------------------------
// Rebuilding the moov
// ---------------------------------------------------------------------------

fn tkhd_alt_offset(b: &[u8]) -> usize {
    if b[8] == 1 { 54 } else { 42 }
}

fn tkhd_duration_offset(b: &[u8]) -> (usize, bool) {
    if b[8] == 1 { (36, true) } else { (28, false) }
}

fn mvhd_duration_offset(b: &[u8]) -> (usize, bool) {
    if b[8] == 1 { (32, true) } else { (24, false) }
}

fn mdhd_duration_offset(b: &[u8]) -> (usize, bool) {
    if b[8] == 1 { (32, true) } else { (24, false) }
}

fn patch_duration(b: &mut [u8], at: usize, wide: bool, v: u64) {
    if wide {
        b[at..at + 8].copy_from_slice(&v.to_be_bytes());
    } else {
        b[at..at + 4].copy_from_slice(&(v.min(u32::MAX as u64) as u32).to_be_bytes());
    }
}

/// A track as the rebuilt moov wants it.
#[derive(Default)]
struct TrakPlan {
    media_duration: Option<u64>,
    tkhd_duration: Option<u64>,
    tkhd_flags: Option<u32>,
    alternate_group: Option<u16>,
    /// Replacement edit list (None keeps whatever the source had).
    edits: Option<Vec<Edit>>,
    /// Replacement sample tables and their sample count (None keeps `stbl`).
    tables: Option<(Tables, u32)>,
}

fn rebuild_stbl(stbl: &[u8], plan: &TrakPlan) -> Result<Vec<u8>> {
    let Some((t, count)) = &plan.tables else {
        return Ok(stbl.to_vec());
    };
    let kids = children(stbl, 8, stbl.len())?;
    let mut before = vec![];
    let mut after = vec![];
    let mut replaced = false;
    for b in &kids {
        match &b.kind {
            b"stts" | b"ctts" | b"stss" | b"stsz" | b"stsc" | b"stco" | b"co64" => {
                if !replaced {
                    replaced = true;
                    before.extend_from_slice(&stbl_table_boxes(t, *count));
                }
            }
            _ if !replaced => before.extend_from_slice(slice(stbl, b)),
            _ => after.extend_from_slice(slice(stbl, b)),
        }
    }
    if !replaced {
        before.extend_from_slice(&stbl_table_boxes(t, *count));
    }
    before.extend_from_slice(&after);
    let mut out = vec![];
    push_box(&mut out, b"stbl", &before);
    Ok(out)
}

fn rebuild_minf(minf: &[u8], plan: &TrakPlan) -> Result<Vec<u8>> {
    let kids = children(minf, 8, minf.len())?;
    let mut out = vec![];
    for b in &kids {
        if &b.kind == b"stbl" {
            out.extend_from_slice(&rebuild_stbl(slice(minf, b), plan)?);
        } else {
            out.extend_from_slice(slice(minf, b));
        }
    }
    let mut boxed = vec![];
    push_box(&mut boxed, b"minf", &out);
    Ok(boxed)
}

fn rebuild_mdia(mdia: &[u8], plan: &TrakPlan) -> Result<Vec<u8>> {
    let kids = children(mdia, 8, mdia.len())?;
    let mut out = vec![];
    for b in &kids {
        match &b.kind {
            b"mdhd" => {
                let mut m = slice(mdia, b).to_vec();
                if let Some(d) = plan.media_duration {
                    let (at, wide) = mdhd_duration_offset(&m);
                    patch_duration(&mut m, at, wide, d);
                }
                out.extend_from_slice(&m);
            }
            b"minf" => out.extend_from_slice(&rebuild_minf(slice(mdia, b), plan)?),
            _ => out.extend_from_slice(slice(mdia, b)),
        }
    }
    let mut boxed = vec![];
    push_box(&mut boxed, b"mdia", &out);
    Ok(boxed)
}

fn rebuild_trak(trak: &[u8], plan: &TrakPlan) -> Result<Vec<u8>> {
    let kids = children(trak, 8, trak.len())?;
    let mut out = vec![];
    for b in &kids {
        match &b.kind {
            b"tkhd" => {
                let mut t = slice(trak, b).to_vec();
                if let Some(flags) = plan.tkhd_flags {
                    t[9] = (flags >> 16) as u8;
                    t[10] = (flags >> 8) as u8;
                    t[11] = flags as u8;
                }
                if let Some(g) = plan.alternate_group {
                    let at = tkhd_alt_offset(&t);
                    t[at..at + 2].copy_from_slice(&g.to_be_bytes());
                }
                if let Some(d) = plan.tkhd_duration {
                    let (at, wide) = tkhd_duration_offset(&t);
                    patch_duration(&mut t, at, wide, d);
                }
                out.extend_from_slice(&t);
                if let Some(edits) = &plan.edits {
                    out.extend_from_slice(&edts_box(edits));
                }
            }
            b"edts" => {
                if plan.edits.is_none() {
                    out.extend_from_slice(slice(trak, b));
                }
            }
            b"mdia" => out.extend_from_slice(&rebuild_mdia(slice(trak, b), plan)?),
            _ => out.extend_from_slice(slice(trak, b)),
        }
    }
    let mut boxed = vec![];
    push_box(&mut boxed, b"trak", &out);
    Ok(boxed)
}

fn rebuild_moov(moov: &[u8], plans: &[TrakPlan], duration: u64) -> Result<Vec<u8>> {
    let kids = children(moov, 8, moov.len())?;
    let mut out = vec![];
    let mut track = 0;
    for b in &kids {
        match &b.kind {
            b"mvhd" => {
                let mut m = slice(moov, b).to_vec();
                let (at, wide) = mvhd_duration_offset(&m);
                patch_duration(&mut m, at, wide, duration);
                out.extend_from_slice(&m);
            }
            b"trak" => {
                let empty = TrakPlan::default();
                let plan = plans.get(track).unwrap_or(&empty);
                out.extend_from_slice(&rebuild_trak(slice(moov, b), plan)?);
                track += 1;
            }
            b"mvex" => {}
            _ => out.extend_from_slice(slice(moov, b)),
        }
    }
    let mut boxed = vec![];
    push_box(&mut boxed, b"moov", &out);
    Ok(boxed)
}

// ---------------------------------------------------------------------------
// The finalise
// ---------------------------------------------------------------------------

fn scale(v: u64, from: u32, to: u32) -> u64 {
    if from == 0 {
        0
    } else {
        ((v as u128 * to as u128 + from as u128 / 2) / from as u128) as u64
    }
}

/// SIGKILL this process: the test fault points.
fn kill_now() -> ! {
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    // Only reached if the signal was blocked: never continue the finalise.
    std::process::abort();
}

// Until the append is durable, never touch the existing fragment headers.
// ENOSPC can leave a partial moov at EOF; remove only those new bytes so
// readers can still scan all completed fragments. A disconnected device may
// reject rollback too: preserve the original error and leave recovery to the
// caller rather than trying more writes to existing media.
fn append_transaction(
    file: &mut File,
    append: impl FnOnce(&mut File) -> std::io::Result<()>,
) -> Result<()> {
    let len = file.metadata()?.len();
    if let Err(error) = append(file) {
        let rollback = file.set_len(len).and_then(|()| file.sync_all());
        return match rollback {
            Ok(()) => Err(error).context("appending movie index; original fragments preserved"),
            Err(rollback) => Err(error).context(format!(
                "appending movie index; could not remove partial index: {rollback}"
            )),
        };
    }
    Ok(())
}

/// Turn a fragmented recording into a regular movie, in place, and apply the
/// track layout. Media bytes are never moved or rewritten.
///
/// `layout.audio_priming` and `layout.default_audio` are in moov audio order.
/// A track's priming is given in samples at 48 kHz and scaled to the track's
/// own timescale here. `video_end` caps audio presentation (after priming),
/// without trimming AAC samples or extending audio tracks that end earlier.
///
/// `keep` copies the file before the append, so a test can compare the
/// fragmented and finalised forms of the same recording.
pub fn finalize(
    path: &Path,
    layout: &Layout,
    crash_at: CrashAt,
    keep: Option<&Path>,
    video_end: Option<f64>,
) -> Result<FinalizeReport> {
    let started = Instant::now();
    let (mut file, boxes, len) = scan(path)?;
    let moov_ref = *boxes
        .iter()
        .rfind(|b| b.kind == MOOV)
        .context("no moov box to finalise")?;
    let moov = read_at(&mut file, moov_ref.start, moov_ref.size)?;
    let init = parse_moov(&moov)?;
    let fragments = boxes.iter().filter(|b| b.kind == MOOF).count();
    let defragmented = init.has_mvex || fragments > 0;

    // Samples per track id, in fragment order.
    let mut per_track: HashMap<u32, Vec<Sample>> = HashMap::new();
    if defragmented {
        let mut firsts = HashMap::new();
        for b in boxes.iter().filter(|b| b.kind == MOOF) {
            let buf = read_at(&mut file, b.start, b.size)?;
            for (id, decode_time, samples) in parse_moof(&buf, b.start, &init.trex)? {
                let video = init
                    .tracks
                    .iter()
                    .any(|t| t.info.track_id == id && t.info.handler == Handler::Video);
                append_fragment(
                    per_track.entry(id).or_default(),
                    &mut firsts,
                    id,
                    decode_time,
                    samples,
                    video,
                )?;
            }
        }
    }

    let mut plans: Vec<TrakPlan> = vec![];
    let mut track_durations: Vec<u64> = vec![];
    let mut total_samples = 0u64;
    let mut audio_seen = 0usize;
    for t in &init.tracks {
        let mut plan = TrakPlan::default();
        let samples = per_track.get(&t.info.track_id);
        let media_duration = match samples {
            Some(samples) => {
                let mut samples = if t.info.handler == Handler::Audio {
                    sane_durations(samples, t.info.timescale)
                } else {
                    samples.to_vec()
                };
                if t.info.handler == Handler::Video
                    && let Some(end) = video_end
                {
                    let end = (end.max(0.0) * t.info.timescale as f64).round() as u64;
                    extend_last_sample(&mut samples, end);
                }
                total_samples += samples.len() as u64;
                let tables = build_tables(&samples);
                plan.tables = Some((tables, samples.len() as u32));
                Some(media_duration(&samples))
            }
            // A regular movie keeps its tables; a track with no fragments at
            // all keeps its (empty) init stbl.
            None if !defragmented => Some(t.info.duration),
            None => Some(0),
        };
        let Some(media_duration) = media_duration else {
            continue;
        };
        plan.media_duration = Some(media_duration);
        let tkhd_duration = scale(media_duration, t.info.timescale, init.movie_timescale);
        plan.tkhd_duration = Some(tkhd_duration);

        if t.info.handler == Handler::Audio {
            let index = audio_seen;
            audio_seen += 1;
            if let Some(group) = layout.audio_alternate_group {
                plan.alternate_group = Some(group);
            }
            if let Some(default) = layout.default_audio {
                // The default track stays enabled; the others are disabled
                // alternatives in the same group.
                plan.tkhd_flags = Some(if index == default { 0x7 } else { 0x6 });
            }
            let priming_48k = layout.audio_priming.get(index).copied().unwrap_or(0);
            let priming = scale(priming_48k, 48_000, t.info.timescale);
            // Keep the existing priming policy for empty/too-short media.
            let priming = if priming < media_duration { priming } else { 0 };
            let shown = scale(
                media_duration - priming,
                t.info.timescale,
                init.movie_timescale,
            );
            let capped = video_end
                .map(|end| (end.max(0.0) * init.movie_timescale as f64).round() as u64)
                .map_or(shown, |end| shown.min(end));
            // Even with zero priming an edit is needed to hide the tail of
            // the AAC frame crossing the take end. Leave media duration and
            // sample tables intact so that frame remains decodable.
            if priming > 0 || capped < shown {
                plan.edits = Some(vec![Edit {
                    segment_duration: capped,
                    media_time: priming as i64,
                }]);
                plan.tkhd_duration = Some(capped);
            }
        }
        track_durations.push(plan.tkhd_duration.unwrap_or(tkhd_duration));
        plans.push(plan);
    }
    let movie_duration = track_durations.iter().copied().max().unwrap_or(0);
    let new_moov = rebuild_moov(&moov, &plans, movie_duration)?;

    if let Some(keep) = keep {
        std::fs::copy(path, keep)
            .with_context(|| format!("keeping the fragmented copy at {}", keep.display()))?;
    }

    // 1. Append the regular moov. Until the flip, the file has two moovs and
    //    players read the fragmented one.
    let mut appended = OpenOptions::new().append(true).open(path)?;
    append_transaction(&mut appended, |file| {
        file.write_all(&new_moov)?;
        file.sync_all()
    })?;
    drop(appended);
    if crash_at == CrashAt::BeforeFlip {
        kill_now();
    }

    // 2. Flip every moof (and the stale `mfra` index) to `free`, then the old
    //    init moov. The order matters for a crash *inside* the flip: while the
    //    init moov is still there the file is a playable fragmented movie (a
    //    moof already flipped is a gap, not a broken header), and the last
    //    step — one 4-byte write — makes the appended regular moov the only
    //    one. The other order would leave stray `moof` boxes in a file whose
    //    player has already passed the init moov, which ffmpeg refuses to
    //    parse ("trun track id unknown").
    let mut f = OpenOptions::new().write(true).open(path)?;
    let mut flips: Vec<BoxRef> = boxes
        .iter()
        .filter(|b| b.kind == MOOF || b.kind == *b"mfra")
        .copied()
        .collect();
    // A crash in the middle leaves the first fragments (the first video one
    // among them) and the init moov, so the file still plays up to there;
    // flipping from the front could remove every video fragment.
    if crash_at == CrashAt::MidFlip {
        let keep = flips.len() / 2;
        flips.drain(..keep);
    }
    for b in &flips {
        f.seek(SeekFrom::Start(b.start + 4))?;
        f.write_all(&FREE)?;
    }
    if crash_at == CrashAt::MidFlip {
        f.sync_all()?;
        drop(f);
        kill_now();
    }
    f.seek(SeekFrom::Start(moov_ref.start + 4))?;
    f.write_all(&FREE)?;
    f.sync_all()?;
    drop(f);

    Ok(FinalizeReport {
        defragmented,
        fragments,
        tracks: plans.len(),
        samples: total_samples,
        input_bytes: len,
        moov_bytes: new_moov.len(),
        millis: started.elapsed().as_millis(),
    })
}

#[cfg(test)]
#[path = "mp4/tests.rs"]
mod tests;
