//! Tests for the MP4 box surgery, on synthetic boxes and files.
//!
//! The fixtures are the smallest structurally valid movies this module has to
//! understand: an init `moov` with two or more tracks and `moof`/`mdat` pairs
//! exactly as qtmux writes them (one `trun` per track fragment, per-sample
//! durations/sizes/flags, `data_offset` relative to the moof start).

use super::*;

// --- box builders ---------------------------------------------------------

fn full_flags(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = vec![version];
    b.extend_from_slice(&flags.to_be_bytes()[1..]);
    b.extend_from_slice(body);
    let mut out = vec![];
    push_box(&mut out, kind, &b);
    out
}

fn mvhd(timescale: u32, duration: u32) -> Vec<u8> {
    let mut body = vec![];
    for v in [0u32, 0, timescale, duration] {
        body.extend_from_slice(&v.to_be_bytes());
    }
    body.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
    body.extend_from_slice(&[1, 0, 0, 0]); // volume 1.0 + reserved
    body.extend_from_slice(&[0; 8]); // reserved
    body.extend_from_slice(&[0x00, 0x01, 0x00, 0x00, 0, 0, 0, 0]);
    body.extend_from_slice(&[0; 28]); // rest of the matrix
    body.extend_from_slice(&[0; 24]); // pre_defined
    body.extend_from_slice(&4u32.to_be_bytes()); // next track id
    full_flags(b"mvhd", 0, 0, &body)
}

fn tkhd(track_id: u32, flags: u32, alt: u16, duration: u32) -> Vec<u8> {
    let mut body = vec![];
    for v in [0u32, 0, track_id, 0, duration] {
        body.extend_from_slice(&v.to_be_bytes());
    }
    body.extend_from_slice(&[0; 8]); // reserved
    body.extend_from_slice(&0u16.to_be_bytes()); // layer
    body.extend_from_slice(&alt.to_be_bytes());
    body.extend_from_slice(&[0, 0, 0, 0]); // volume + reserved
    body.extend_from_slice(&[0; 36]); // matrix
    body.extend_from_slice(&[0; 8]); // width/height
    full_flags(b"tkhd", 0, flags, &body)
}

fn mdhd(timescale: u32, duration: u32) -> Vec<u8> {
    let mut body = vec![];
    for v in [0u32, 0, timescale, duration] {
        body.extend_from_slice(&v.to_be_bytes());
    }
    body.extend_from_slice(&[0x55, 0xc4, 0, 0]); // language "und" + pre_defined
    full_flags(b"mdhd", 0, 0, &body)
}

fn hdlr(kind: &[u8; 4]) -> Vec<u8> {
    let mut body = vec![0, 0, 0, 0];
    body.extend_from_slice(kind);
    body.extend_from_slice(&[0; 12]);
    body.extend_from_slice(b"handler\0");
    full_flags(b"hdlr", 0, 0, &body)
}

fn stsd() -> Vec<u8> {
    let mut body = vec![0, 0, 0, 1u8];
    // A single, deliberately opaque sample entry: the finalise copies stsd.
    push_box(&mut body, b"fake", &[0; 8]);
    full_flags(b"stsd", 0, 0, &body)
}

fn empty_tables() -> Vec<u8> {
    let mut out = stsd();
    for (kind, extra) in [
        (b"stts", 0usize),
        (b"stsc", 0),
        (b"stco", 0),
    ] {
        let mut body = vec![0, 0, 0, 0]; // entry count 0
        body.extend_from_slice(&[0; 0][..extra]);
        out.extend_from_slice(&full_flags(kind, 0, 0, &body));
    }
    // stsz with a zero sample count.
    out.extend_from_slice(&full_flags(b"stsz", 0, 0, &[0, 0, 0, 0, 0, 0, 0, 0]));
    out
}

fn stbl_with_tables() -> Vec<u8> {
    // A "regular movie" stbl: one chunk, two samples of 10 bytes.
    let mut out = stsd();
    out.extend_from_slice(&full_flags(
        b"stts",
        0,
        0,
        &[0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0x03, 0xe8],
    ));
    out.extend_from_slice(&full_flags(
        b"stsc",
        0,
        0,
        &[0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 1],
    ));
    out.extend_from_slice(&full_flags(
        b"stsz",
        0,
        0,
        &[0, 0, 0, 10, 0, 0, 0, 2],
    ));
    out.extend_from_slice(&full_flags(b"stco", 0, 0, &[0, 0, 0, 1, 0, 0, 0x10, 0x00]));
    out
}

struct TrackFixture {
    id: u32,
    handler: [u8; 4],
    timescale: u32,
    duration: u32,
    flags: u32,
    tables: Vec<u8>,
}

fn trak(t: &TrackFixture) -> Vec<u8> {
    let mut body = tkhd(t.id, t.flags, 0, t.duration);
    let mut mdia = mdhd(t.timescale, t.duration);
    mdia.extend_from_slice(&hdlr(&t.handler));
    let mut minf = full_flags(b"vmhd", 0, 1, &[0; 8]);
    minf.extend_from_slice(&vec![0, 0, 0, 8, b'd', b'i', b'n', b'f']);
    let mut stbl = vec![];
    push_box(&mut stbl, b"stbl", &t.tables);
    minf.extend_from_slice(&stbl);
    let mut minf_box = vec![];
    push_box(&mut minf_box, b"minf", &minf);
    mdia.extend_from_slice(&minf_box);
    let mut mdia_box = vec![];
    push_box(&mut mdia_box, b"mdia", &mdia);
    body.extend_from_slice(&mdia_box);
    let mut out = vec![];
    push_box(&mut out, b"trak", &body);
    out
}

fn init_moov(movie_timescale: u32, tracks: &[TrackFixture]) -> Vec<u8> {
    let mut body = mvhd(movie_timescale, 0);
    for t in tracks {
        body.extend_from_slice(&trak(t));
    }
    // mvex with trex defaults, as a fragmented init moov has.
    let mut mvex = full_flags(b"mehd", 0, 0, &[0, 0, 0, 0]);
    for t in tracks {
        let mut trex = vec![];
        trex.extend_from_slice(&t.id.to_be_bytes());
        trex.extend_from_slice(&1u32.to_be_bytes()); // sample description
        trex.extend_from_slice(&[0; 12]); // duration/size/flags defaults 0
        mvex.extend_from_slice(&full_flags(b"trex", 0, 0, &trex));
    }
    let mut mvex_box = vec![];
    push_box(&mut mvex_box, b"mvex", &mvex);
    body.extend_from_slice(&mvex_box);
    let mut out = vec![];
    push_box(&mut out, b"moov", &body);
    out
}

fn ftyp() -> Vec<u8> {
    let mut body = b"isom".to_vec();
    body.extend_from_slice(&[0, 0, 2, 0]);
    body.extend_from_slice(b"isomiso2mp41");
    let mut out = vec![];
    push_box(&mut out, b"ftyp", &body);
    out
}

/// One sample: duration (media units), size, sample flags, composition offset.
type S = (u32, u32, u32, i32);

struct TrafSpec<'a> {
    track_id: u32,
    /// (duration, size, flags) defaults written into the tfhd.
    defaults: Option<(u32, u32, u32)>,
    samples: &'a [S],
    /// Write a `trun` `data_offset` (relative to the moof) or not.
    data_offset: bool,
}

fn tfhd(track_id: u32, defaults: Option<(u32, u32, u32)>) -> Vec<u8> {
    let mut flags = 0u32;
    let mut body = track_id.to_be_bytes().to_vec();
    if let Some((d, s, f)) = defaults {
        flags |= 0x8 | 0x10 | 0x20;
        body.extend_from_slice(&d.to_be_bytes());
        body.extend_from_slice(&s.to_be_bytes());
        body.extend_from_slice(&f.to_be_bytes());
    }
    full_flags(b"tfhd", 0, flags, &body)
}

fn tfdt(time: u32) -> Vec<u8> {
    full_flags(b"tfdt", 0, 0, &time.to_be_bytes())
}

fn trun(samples: &[S], data_offset: Option<i32>) -> Vec<u8> {
    let mut flags = 0x100 | 0x200 | 0x400; // duration, size, flags
    let has_cts = samples.iter().any(|s| s.3 != 0);
    if has_cts {
        flags |= 0x800;
    }
    let mut body = (samples.len() as u32).to_be_bytes().to_vec();
    if let Some(d) = data_offset {
        flags |= 0x1;
        body.extend_from_slice(&d.to_be_bytes());
    }
    for (duration, size, sf, cts) in samples {
        body.extend_from_slice(&duration.to_be_bytes());
        body.extend_from_slice(&size.to_be_bytes());
        body.extend_from_slice(&sf.to_be_bytes());
        if has_cts {
            body.extend_from_slice(&cts.to_be_bytes());
        }
    }
    full_flags(b"trun", if has_cts { 1 } else { 0 }, flags, &body)
}

/// A `moof` whose truns point at the mdat that follows it.
fn moof_at(moof_offset: u64, trafs: &[TrafSpec]) -> Vec<u8> {
    // Two passes: the size decides the data offsets relative to the moof.
    fn build(trafs: &[TrafSpec], size: u64) -> Vec<u8> {
        let mut body = full_flags(b"mfhd", 0, 0, &1u32.to_be_bytes());
        for t in trafs {
            let mut traf = tfhd(t.track_id, t.defaults);
            traf.extend_from_slice(&tfdt(0));
            let off = t.data_offset.then_some((size + 8) as i32);
            traf.extend_from_slice(&trun(t.samples, off));
            let mut traf_box = vec![];
            push_box(&mut traf_box, b"traf", &traf);
            body.extend_from_slice(&traf_box);
        }
        let mut out = vec![];
        push_box(&mut out, b"moof", &body);
        out
    }
    let first = build(trafs, 0);
    let _ = moof_offset;
    build(trafs, first.len() as u64)
}

fn mdat(data: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    push_box(&mut out, b"mdat", data);
    out
}

fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rec-rs-mp4-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.mp4"));
    std::fs::write(&path, bytes).unwrap();
    path
}

fn top_level(path: &std::path::Path) -> Vec<[u8; 4]> {
    let (_, boxes, _) = scan(path).unwrap();
    boxes.iter().map(|b| b.kind).collect()
}

fn opt_flags(sync: bool) -> u32 {
    if sync { 0x0200_0000 } else { 0x0101_0000 }
}

// --- tests ----------------------------------------------------------------

#[test]
fn parses_a_moof_the_way_qtmux_writes_it() {
    let samples: [S; 3] = [
        (100, 10, opt_flags(true), 0),
        (100, 11, opt_flags(false), 0),
        (100, 12, opt_flags(false), -1),
    ];
    let trafs = [TrafSpec {
        track_id: 7,
        defaults: Some((100, 0, 0)),
        samples: &samples,
        data_offset: true,
    }];
    let moof = moof_at(1000, &trafs);
    let out = parse_moof(&moof, 1000, &HashMap::new()).unwrap();
    assert_eq!(out.len(), 1);
    let (id, got) = &out[0];
    assert_eq!(*id, 7);
    assert_eq!(got.len(), 3);
    // data_offset is the moof size + the mdat header, from the moof start.
    assert_eq!(got[0].offset, 1000 + moof.len() as u64 + 8);
    assert_eq!(got[1].offset, got[0].offset + 10);
    assert_eq!(got[2].offset, got[1].offset + 11);
    assert_eq!(got[2].size, 12);
    assert_eq!(got[0].duration, 100);
    assert!(!got[1].sync && !got[2].sync);
    assert!(got[0].sync);
    assert_eq!(got[2].cts, -1);
}

#[test]
fn a_traf_without_a_data_offset_continues_the_previous_one() {
    let video: [S; 2] = [(100, 10, opt_flags(true), 0), (100, 10, opt_flags(false), 0)];
    let audio: [S; 2] = [(1024, 5, 0, 0), (1024, 6, 0, 0)];
    let trafs = [
        TrafSpec {
            track_id: 1,
            defaults: None,
            samples: &video,
            data_offset: true,
        },
        TrafSpec {
            track_id: 2,
            defaults: None,
            samples: &audio,
            data_offset: false,
        },
    ];
    let moof = moof_at(500, &trafs);
    let out = parse_moof(&moof, 500, &HashMap::new()).unwrap();
    let v = &out.iter().find(|(id, _)| *id == 1).unwrap().1;
    let a = &out.iter().find(|(id, _)| *id == 2).unwrap().1;
    assert_eq!(v[0].offset, 500 + moof.len() as u64 + 8);
    // The second traf starts where the first one's data ended.
    assert_eq!(a[0].offset, v[1].offset + 10);
    assert_eq!(a[1].offset, a[0].offset + 5);
}

#[test]
fn tfhd_defaults_and_trex_fill_missing_sample_fields() {
    // No per-sample fields in the trun: the tfhd supplies the duration and the
    // trex supplies the size and flags it does not have.
    let trun = full_flags(b"trun", 0, 0x1, &[0, 0, 0, 2, 0, 0, 0, 0]);
    // flags 0x8 = default_sample_duration only.
    let mut tfhd_body = 1u32.to_be_bytes().to_vec();
    tfhd_body.extend_from_slice(&256u32.to_be_bytes());
    let mut traf = full_flags(b"tfhd", 0, 0x8, &tfhd_body);
    traf.extend_from_slice(&tfdt(0));
    traf.extend_from_slice(&trun);
    let mut body = full_flags(b"mfhd", 0, 0, &[0; 4]);
    let mut traf_box = vec![];
    push_box(&mut traf_box, b"traf", &traf);
    body.extend_from_slice(&traf_box);
    let mut moof = vec![];
    push_box(&mut moof, b"moof", &body);
    let mut trex = HashMap::new();
    trex.insert(
        1u32,
        Trex {
            duration: 512,
            size: 99,
            flags: 0x0101_0000,
        },
    );
    let out = parse_moof(&moof, 0, &trex).unwrap();
    let (_, samples) = &out[0];
    assert_eq!(samples.len(), 2);
    assert_eq!(samples[0].duration, 256, "tfhd duration wins over trex");
    assert_eq!(samples[0].size, 99, "trex size");
    assert!(!samples[0].sync, "trex flags say non-sync");
}

#[test]
fn tables_cover_co64_ctts_and_sync_samples() {
    let big = 5u64 << 30; // 5 GiB, above what stco can hold
    let samples = vec![
        Sample { offset: big, size: 10, duration: 100, cts: 0, sync: true },
        Sample { offset: big + 10, size: 11, duration: 100, cts: 3, sync: false },
        Sample { offset: big + 21, size: 12, duration: 101, cts: 3, sync: true },
    ];
    let t = build_tables(&samples);
    assert!(t.co64);
    assert_eq!(t.stts, vec![(2, 100), (1, 101)]);
    assert_eq!(t.stss, vec![1, 3]);
    assert_eq!(t.ctts, Some((0, vec![(1, 0), (2, 3)])));
    assert_eq!(t.sizes, vec![10, 11, 12]);
    assert_eq!(t.chunks.len(), 1, "contiguous samples form one chunk");
    assert_eq!(t.stsc, vec![(1, 3)]);
    let bytes = stbl_table_boxes(&t, 3);
    assert!(bytes.windows(4).any(|w| w == b"co64"), "co64 for a 5 GiB offset");
    assert!(!bytes.windows(4).any(|w| w == b"stco"));
    // The offset is there, as a 64-bit value.
    assert!(bytes.windows(8).any(|w| w == big.to_be_bytes()));
}

#[test]
fn a_negative_composition_offset_uses_ctts_version_1() {
    let samples = vec![
        Sample { offset: 0, size: 1, duration: 1, cts: -2, sync: true },
        Sample { offset: 1, size: 1, duration: 1, cts: 0, sync: true },
    ];
    let t = build_tables(&samples);
    assert_eq!(t.ctts, Some((1, vec![(1, -2), (1, 0)])));
    let bytes = stbl_table_boxes(&t, 2);
    let at = bytes.windows(4).position(|w| w == b"ctts").unwrap();
    assert_eq!(bytes[at - 4 + 8], 1, "ctts version 1");
}

#[test]
fn all_sync_samples_need_no_stss() {
    let samples = vec![
        Sample { offset: 0, size: 4, duration: 1, cts: 0, sync: true },
        Sample { offset: 4, size: 4, duration: 1, cts: 0, sync: true },
    ];
    let t = build_tables(&samples);
    assert!(t.stss.is_empty());
    assert_eq!(t.fixed_size, Some(4));
    let bytes = stbl_table_boxes(&t, 2);
    assert!(!bytes.windows(4).any(|w| w == b"stss"));
    // The fixed-size stsz carries the size and the count.
    let at = bytes.windows(4).position(|w| w == b"stsz").unwrap();
    assert_eq!(be32(&bytes, at + 8).unwrap(), 4);
    assert_eq!(be32(&bytes, at + 12).unwrap(), 2);
}

/// A movie with a video track and three audio tracks, two fragments for the
/// video and one for each audio track, as the recorder produces.
fn fragmented_fixture() -> (Vec<u8>, Vec<Vec<u8>>) {
    let tracks = vec![
        TrackFixture { id: 1, handler: *b"vide", timescale: 3000, duration: 0, flags: 0x7, tables: empty_tables() },
        TrackFixture { id: 2, handler: *b"soun", timescale: 48000, duration: 0, flags: 0x7, tables: empty_tables() },
        TrackFixture { id: 3, handler: *b"soun", timescale: 48000, duration: 0, flags: 0x7, tables: empty_tables() },
        TrackFixture { id: 4, handler: *b"soun", timescale: 48000, duration: 0, flags: 0x7, tables: empty_tables() },
    ];
    let mut out = ftyp();
    out.extend_from_slice(&init_moov(1000, &tracks));

    let mut payloads: Vec<Vec<u8>> = vec![];
    // Fragment 1: two video samples, half of each audio track.
    let video1: [S; 2] = [(100, 10, opt_flags(true), 0), (100, 11, opt_flags(false), 0)];
    let mix1: [S; 2] = [(1024, 5, 0, 0), (1024, 6, 0, 0)];
    let sys1: [S; 2] = [(1024, 7, 0, 0), (1024, 8, 0, 0)];
    let trafs1 = [
        TrafSpec { track_id: 1, defaults: None, samples: &video1, data_offset: true },
        TrafSpec { track_id: 2, defaults: None, samples: &mix1, data_offset: false },
        TrafSpec { track_id: 3, defaults: None, samples: &sys1, data_offset: false },
    ];
    let moof1 = moof_at(out.len() as u64, &trafs1);
    let mut data1: Vec<u8> = (0..10).collect();
    data1.extend(100..111);
    data1.extend(200..205);
    data1.extend(210..216);
    data1.extend(44..51);
    data1.extend(60..68);
    payloads.push(data1.clone());
    out.extend_from_slice(&moof1);
    out.extend_from_slice(&mdat(&data1));

    // Fragment 2: the last two video samples and the rest of every audio track.
    let video2: [S; 2] = [(100, 12, opt_flags(false), 0), (100, 13, opt_flags(true), 0)];
    let mix2: [S; 2] = [(1024, 7, 0, 0), (1024, 8, 0, 0)];
    let sys2: [S; 2] = [(1024, 9, 0, 0), (1024, 10, 0, 0)];
    let mic2: [S; 4] = [(1024, 9, 0, 0), (1024, 10, 0, 0), (1024, 11, 0, 0), (1024, 12, 0, 0)];
    let trafs2 = [
        TrafSpec { track_id: 1, defaults: None, samples: &video2, data_offset: true },
        TrafSpec { track_id: 2, defaults: None, samples: &mix2, data_offset: false },
        TrafSpec { track_id: 3, defaults: None, samples: &sys2, data_offset: false },
        TrafSpec { track_id: 4, defaults: None, samples: &mic2, data_offset: false },
    ];
    let moof2 = moof_at(out.len() as u64, &trafs2);
    let mut data2: Vec<u8> = (20..32).collect();
    data2.extend(70..79);
    data2.extend(90..100);
    data2.extend(110..119);
    data2.extend(120..130);
    data2.extend(130..141);
    data2.extend(150..162);
    payloads.push(data2.clone());
    out.extend_from_slice(&moof2);
    out.extend_from_slice(&mdat(&data2));
    (out, payloads)
}

fn layout() -> Layout {
    Layout {
        audio_alternate_group: Some(1),
        default_audio: Some(0),
        // 2048 samples at 48 kHz on each audio track.
        audio_priming: vec![2048, 2048, 2048],
    }
}

/// The `stco`/`co64` offsets of the nth `trak` in a moov.
fn chunk_offsets(moov: &[u8], index: usize) -> Vec<u64> {
    let kids = children(moov, 8, moov.len()).unwrap();
    let trak = kids.iter().filter(|b| &b.kind == b"trak").nth(index).unwrap();
    let tk = children(moov, trak.body_start() as usize, (trak.start + trak.size) as usize).unwrap();
    let mdia = find(&tk, b"mdia").unwrap();
    let mdia_buf = slice(moov, mdia);
    let mk = children(mdia_buf, 8, mdia_buf.len()).unwrap();
    let minf = find(&mk, b"minf").unwrap();
    let minf_buf = slice(mdia_buf, minf);
    let mk2 = children(minf_buf, 8, minf_buf.len()).unwrap();
    let stbl = find(&mk2, b"stbl").unwrap();
    let stbl_buf = slice(minf_buf, stbl);
    let sk = children(stbl_buf, 8, stbl_buf.len()).unwrap();
    let Some(co) = find(&sk, b"stco").or_else(|| find(&sk, b"co64")) else {
        return vec![];
    };
    let b = slice(stbl_buf, co);
    let n = be32(b, 12).unwrap() as usize;
    (0..n)
        .map(|i| {
            if &co.kind == b"co64" {
                be64(b, 16 + i * 8).unwrap()
            } else {
                be32(b, 16 + i * 4).unwrap() as u64
            }
        })
        .collect()
}

#[test]
fn finalize_builds_a_regular_movie_and_keeps_the_media_bytes() {
    let (bytes, payloads) = fragmented_fixture();
    let path = write_temp("finalize", &bytes);
    let report = finalize(&path, &layout(), CrashAt::None, None).unwrap();
    assert!(report.defragmented);
    assert_eq!(report.tracks, 4);
    assert_eq!(report.samples, 4 + 4 + 4 + 4);
    assert_eq!(report.fragments, 2);

    // Layout: ftyp, free (the old init moov), mdat, free (old moof), mdat,
    // moov (the regular one, appended).
    let kinds = top_level(&path);
    assert_eq!(kinds[0], *b"ftyp");
    assert_eq!(kinds[1], *b"free");
    assert!(!kinds.contains(b"moof"), "{kinds:?}");
    assert_eq!(kinds.last(), Some(b"moov"));
    assert_eq!(kinds.iter().filter(|k| **k == *b"moov").count(), 1);
    assert_eq!(kinds.iter().filter(|k| **k == *b"free").count(), 3);

    // The sample data is untouched: the same bytes at the same offsets.
    let after = std::fs::read(&path).unwrap();
    for (i, payload) in payloads.iter().enumerate() {
        let from = bytes
            .windows(payload.len())
            .position(|w| w == payload.as_slice())
            .unwrap();
        assert_eq!(&after[from..from + payload.len()], payload.as_slice(), "fragment {i}");
    }

    let info = read(&path).unwrap();
    assert!(!info.fragmented);
    assert_eq!(info.movie_timescale, 1000);
    assert_eq!(info.tracks.len(), 4);
    assert_eq!(info.tracks[0].samples, 4);
    assert_eq!(info.tracks[1].samples, 4);
    assert_eq!(info.tracks[2].samples, 4);
    assert_eq!(info.tracks[3].samples, 4);

    // The audio tracks form one group with only the first enabled.
    let audio: Vec<&TrackInfo> = info.tracks.iter().filter(|t| t.handler == Handler::Audio).collect();
    assert_eq!(audio.len(), 3);
    for t in &audio {
        assert_eq!(t.alternate_group, 1);
    }
    assert!(audio[0].enabled, "the mix track stays enabled");
    assert!(!audio[1].enabled && !audio[2].enabled, "stems are disabled alternatives");
    assert!(info.tracks[0].enabled);

    // Every audio track skips its priming with an edit list.
    for t in &audio {
        assert_eq!(
            t.edits,
            vec![Edit { segment_duration: 43, media_time: 2048 }],
            "4 audio frames of 1024 samples minus 2048 priming = 42.67 ms"
        );
    }

    // Chunk offsets point at the original mdat payloads.
    let (_, boxes, _) = scan(&path).unwrap();
    let moov = boxes.last().unwrap();
    let moov_bytes = std::fs::read(&path).unwrap()[moov.start as usize..(moov.start + moov.size) as usize].to_vec();
    let video_chunks = chunk_offsets(&moov_bytes, 0);
    assert_eq!(video_chunks.len(), 2, "one chunk per fragment");
    let first_sample = &payloads[0][0..10];
    let base = bytes
        .windows(first_sample.len())
        .position(|w| w == first_sample)
        .unwrap();
    assert_eq!(video_chunks[0] as usize, base, "first video sample where it was");
    std::fs::remove_file(&path).ok();
}

#[test]
fn a_two_moov_file_still_reads_as_a_movie() {
    // The state between the append and the flip: the appended regular moov is
    // there, the init moov has not been flipped yet. Both parse; `read` picks
    // the last (the regular one), and the fragmented one is still intact for a
    // player that stops at the first.
    let (bytes, _) = fragmented_fixture();
    let path = write_temp("two-moov", &bytes);
    let (mut file, boxes, len) = scan(&path).unwrap();
    let moov_ref = *boxes.iter().find(|b| b.kind == MOOV).unwrap();
    let moov = read_at(&mut file, moov_ref.start, moov_ref.size).unwrap();
    let init = parse_moov(&moov).unwrap();
    let plans: Vec<TrakPlan> = init
        .tracks
        .iter()
        .map(|t| TrakPlan {
            media_duration: Some(t.info.duration),
            tkhd_duration: Some(t.info.duration),
            ..Default::default()
        })
        .collect();
    let new_moov = rebuild_moov(&moov, &plans, 0).unwrap();
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(&new_moov).unwrap();
    f.sync_all().unwrap();
    drop(f);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), len + new_moov.len() as u64);
    let info = read(&path).unwrap();
    assert_eq!(info.tracks.len(), 4);
    // The init moov is still a fragmented one and still parses.
    let first = read_at(&mut File::open(&path).unwrap(), moov_ref.start, moov_ref.size).unwrap();
    assert!(parse_moov(&first).unwrap().has_mvex);
    std::fs::remove_file(&path).ok();
}

#[test]
fn a_regular_movie_is_patched_without_growing_media_offsets() {
    // A non-fragmented file (REC_FRAGMENT_SECS=0): the finalise must still
    // apply the track layout, by appending a patched moov at the end.
    let tracks = vec![
        TrackFixture { id: 1, handler: *b"vide", timescale: 3000, duration: 3000, flags: 0x7, tables: stbl_with_tables() },
        TrackFixture { id: 2, handler: *b"soun", timescale: 48000, duration: 96000, flags: 0x7, tables: stbl_with_tables() },
    ];
    let mut bytes = ftyp();
    let media_at = bytes.len() + 4096; // mdat payload position, set below
    let mut moov_body = mvhd(1000, 3000);
    let mut moov = vec![];
    for t in &tracks {
        moov_body.extend_from_slice(&trak(t));
    }
    push_box(&mut moov, b"moov", &moov_body);
    bytes.extend_from_slice(&moov);
    let _ = media_at;
    let pad = vec![0xabu8; 64];
    bytes.extend_from_slice(&mdat(&pad));
    let path = write_temp("regular", &bytes);
    let report = finalize(&path, &layout(), CrashAt::None, None).unwrap();
    assert!(!report.defragmented);
    let info = read(&path).unwrap();
    assert!(!info.fragmented);
    let audio = info.tracks.iter().find(|t| t.handler == Handler::Audio).unwrap();
    assert!(audio.enabled, "the only audio track is the default");
    assert_eq!(audio.alternate_group, 1);
    assert_eq!(audio.edits.len(), 1);
    assert_eq!(audio.edits[0].media_time, 2048);
    // The unmodified tables survive.
    assert_eq!(info.tracks[0].samples, 2);
    std::fs::remove_file(&path).ok();
}

#[test]
fn priming_is_scaled_to_the_track_timescale() {
    let tracks = vec![TrackFixture { id: 1, handler: *b"soun", timescale: 44100, duration: 0, flags: 0x7, tables: empty_tables() }];
    let path = write_temp("scale", &[ftyp(), init_moov(1000, &tracks)].concat());
    // Two AAC frames at 44.1 kHz: 2 * 1024/48000 s = 42.67 ms worth of media.
    // Put samples into the file through the fragmented path instead: a moof.
    let mut bytes = std::fs::read(&path).unwrap();
    let samples: [S; 10] = [(940, 5, 0, 0); 10];
    let moof = moof_at(bytes.len() as u64, &[TrafSpec {
        track_id: 1,
        defaults: None,
        samples: &samples,
        data_offset: true,
    }]);
    bytes.extend_from_slice(&moof);
    bytes.extend_from_slice(&mdat(&vec![0u8; 50]));
    std::fs::write(&path, &bytes).unwrap();
    let layout = Layout {
        audio_alternate_group: Some(1),
        default_audio: Some(0),
        audio_priming: vec![2048],
    };
    finalize(&path, &layout, CrashAt::None, None).unwrap();
    let info = read(&path).unwrap();
    let audio = &info.tracks[0];
    assert_eq!(audio.timescale, 44100);
    // 2048 samples at 48 kHz = 42.666 ms; at 44.1 kHz that is 1881.6 -> 1882.
    assert_eq!(audio.edits[0].media_time, 1882);
    // Media duration 9400 units; shown = (9400 - 1882) * 1000 / 44100 = 170 ms.
    assert_eq!(audio.edits[0].segment_duration, 170);
    std::fs::remove_file(&path).ok();
}

#[test]
fn keep_copies_the_fragmented_file_before_the_append() {
    let (bytes, _) = fragmented_fixture();
    let path = write_temp("keep", &bytes);
    let keep = path.with_extension("frag.mp4");
    finalize(&path, &layout(), CrashAt::None, Some(&keep)).unwrap();
    let kept = std::fs::read(&keep).unwrap();
    assert_eq!(kept, bytes, "the kept copy is the fragmented file");
    std::fs::remove_file(&keep).ok();
    std::fs::remove_file(&path).ok();
}


