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
    for (kind, extra) in [(b"stts", 0usize), (b"stsc", 0), (b"stco", 0)] {
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
    out.extend_from_slice(&full_flags(b"stsz", 0, 0, &[0, 0, 0, 10, 0, 0, 0, 2]));
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
    minf.extend_from_slice(&[0, 0, 0, 8, b'd', b'i', b'n', b'f']);
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
    moof_at_times(moof_offset, trafs, &vec![Some(0); trafs.len()])
}

fn moof_at_times(moof_offset: u64, trafs: &[TrafSpec], times: &[Option<u64>]) -> Vec<u8> {
    // Two passes: the size decides the data offsets relative to the moof.
    fn build(trafs: &[TrafSpec], times: &[Option<u64>], size: u64) -> Vec<u8> {
        let mut body = full_flags(b"mfhd", 0, 0, &1u32.to_be_bytes());
        for (t, time) in trafs.iter().zip(times) {
            let mut traf = tfhd(t.track_id, t.defaults);
            if let Some(time) = time {
                // Version 1 exercises the full 64-bit decode-time parser.
                traf.extend_from_slice(&full_flags(b"tfdt", 1, 0, &time.to_be_bytes()));
            }
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
    let first = build(trafs, times, 0);
    let _ = moof_offset;
    build(trafs, times, first.len() as u64)
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
    let (id, _decode_time, got) = &out[0];
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
    let video: [S; 2] = [
        (100, 10, opt_flags(true), 0),
        (100, 10, opt_flags(false), 0),
    ];
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
    let v = &out.iter().find(|(id, _, _)| *id == 1).unwrap().2;
    let a = &out.iter().find(|(id, _, _)| *id == 2).unwrap().2;
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
    let (_, _decode_time, samples) = &out[0];
    assert_eq!(samples.len(), 2);
    assert_eq!(samples[0].duration, 256, "tfhd duration wins over trex");
    assert_eq!(samples[0].size, 99, "trex size");
    assert!(!samples[0].sync, "trex flags say non-sync");
}

#[test]
fn tables_cover_co64_ctts_and_sync_samples() {
    let big = 5u64 << 30; // 5 GiB, above what stco can hold
    let samples = vec![
        Sample {
            offset: big,
            size: 10,
            duration: 100,
            cts: 0,
            sync: true,
        },
        Sample {
            offset: big + 10,
            size: 11,
            duration: 100,
            cts: 3,
            sync: false,
        },
        Sample {
            offset: big + 21,
            size: 12,
            duration: 101,
            cts: 3,
            sync: true,
        },
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
    assert!(
        bytes.windows(4).any(|w| w == b"co64"),
        "co64 for a 5 GiB offset"
    );
    assert!(!bytes.windows(4).any(|w| w == b"stco"));
    // The offset is there, as a 64-bit value.
    assert!(bytes.windows(8).any(|w| w == big.to_be_bytes()));
}

#[test]
fn a_negative_composition_offset_uses_ctts_version_1() {
    let samples = vec![
        Sample {
            offset: 0,
            size: 1,
            duration: 1,
            cts: -2,
            sync: true,
        },
        Sample {
            offset: 1,
            size: 1,
            duration: 1,
            cts: 0,
            sync: true,
        },
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
        Sample {
            offset: 0,
            size: 4,
            duration: 1,
            cts: 0,
            sync: true,
        },
        Sample {
            offset: 4,
            size: 4,
            duration: 1,
            cts: 0,
            sync: true,
        },
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
        TrackFixture {
            id: 1,
            handler: *b"vide",
            timescale: 3000,
            duration: 0,
            flags: 0x7,
            tables: empty_tables(),
        },
        TrackFixture {
            id: 2,
            handler: *b"soun",
            timescale: 48000,
            duration: 0,
            flags: 0x7,
            tables: empty_tables(),
        },
        TrackFixture {
            id: 3,
            handler: *b"soun",
            timescale: 48000,
            duration: 0,
            flags: 0x7,
            tables: empty_tables(),
        },
        TrackFixture {
            id: 4,
            handler: *b"soun",
            timescale: 48000,
            duration: 0,
            flags: 0x7,
            tables: empty_tables(),
        },
    ];
    let mut out = ftyp();
    out.extend_from_slice(&init_moov(1000, &tracks));

    let mut payloads: Vec<Vec<u8>> = vec![];
    // Fragment 1: two video samples, half of each audio track.
    let video1: [S; 2] = [
        (100, 10, opt_flags(true), 0),
        (100, 11, opt_flags(false), 0),
    ];
    let mix1: [S; 2] = [(1024, 5, 0, 0), (1024, 6, 0, 0)];
    let sys1: [S; 2] = [(1024, 7, 0, 0), (1024, 8, 0, 0)];
    let trafs1 = [
        TrafSpec {
            track_id: 1,
            defaults: None,
            samples: &video1,
            data_offset: true,
        },
        TrafSpec {
            track_id: 2,
            defaults: None,
            samples: &mix1,
            data_offset: false,
        },
        TrafSpec {
            track_id: 3,
            defaults: None,
            samples: &sys1,
            data_offset: false,
        },
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
    let video2: [S; 2] = [
        (100, 12, opt_flags(false), 0),
        (100, 13, opt_flags(true), 0),
    ];
    let mix2: [S; 2] = [(1024, 7, 0, 0), (1024, 8, 0, 0)];
    let sys2: [S; 2] = [(1024, 9, 0, 0), (1024, 10, 0, 0)];
    let mic2: [S; 4] = [
        (1024, 9, 0, 0),
        (1024, 10, 0, 0),
        (1024, 11, 0, 0),
        (1024, 12, 0, 0),
    ];
    let trafs2 = [
        TrafSpec {
            track_id: 1,
            defaults: None,
            samples: &video2,
            data_offset: true,
        },
        TrafSpec {
            track_id: 2,
            defaults: None,
            samples: &mix2,
            data_offset: false,
        },
        TrafSpec {
            track_id: 3,
            defaults: None,
            samples: &sys2,
            data_offset: false,
        },
        TrafSpec {
            track_id: 4,
            defaults: None,
            samples: &mic2,
            data_offset: false,
        },
    ];
    let moof2 = moof_at_times(
        out.len() as u64,
        &trafs2,
        &[Some(200), Some(2048), Some(2048), Some(0)],
    );
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
    let trak = kids
        .iter()
        .filter(|b| &b.kind == b"trak")
        .nth(index)
        .unwrap();
    let tk = children(
        moov,
        trak.body_start() as usize,
        (trak.start + trak.size) as usize,
    )
    .unwrap();
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
    let report = finalize(&path, &layout(), CrashAt::None, None, None).unwrap();
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
        assert_eq!(
            &after[from..from + payload.len()],
            payload.as_slice(),
            "fragment {i}"
        );
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
    let audio: Vec<&TrackInfo> = info
        .tracks
        .iter()
        .filter(|t| t.handler == Handler::Audio)
        .collect();
    assert_eq!(audio.len(), 3);
    for t in &audio {
        assert_eq!(t.alternate_group, 1);
    }
    assert!(audio[0].enabled, "the mix track stays enabled");
    assert!(
        !audio[1].enabled && !audio[2].enabled,
        "stems are disabled alternatives"
    );
    assert!(info.tracks[0].enabled);

    // Every audio track skips its priming with an edit list.
    for t in &audio {
        assert_eq!(
            t.edits,
            vec![Edit {
                segment_duration: 43,
                media_time: 2048
            }],
            "4 audio frames of 1024 samples minus 2048 priming = 42.67 ms"
        );
    }

    // Chunk offsets point at the original mdat payloads.
    let (_, boxes, _) = scan(&path).unwrap();
    let moov = boxes.last().unwrap();
    let moov_bytes = std::fs::read(&path).unwrap()
        [moov.start as usize..(moov.start + moov.size) as usize]
        .to_vec();
    let video_chunks = chunk_offsets(&moov_bytes, 0);
    assert_eq!(video_chunks.len(), 2, "one chunk per fragment");
    let first_sample = &payloads[0][0..10];
    let base = bytes
        .windows(first_sample.len())
        .position(|w| w == first_sample)
        .unwrap();
    assert_eq!(
        video_chunks[0] as usize, base,
        "first video sample where it was"
    );
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
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        len + new_moov.len() as u64
    );
    let info = read(&path).unwrap();
    assert_eq!(info.tracks.len(), 4);
    // The init moov is still a fragmented one and still parses.
    let first = read_at(
        &mut File::open(&path).unwrap(),
        moov_ref.start,
        moov_ref.size,
    )
    .unwrap();
    assert!(parse_moov(&first).unwrap().has_mvex);
    std::fs::remove_file(&path).ok();
}

#[test]
fn a_regular_movie_is_patched_without_growing_media_offsets() {
    // A non-fragmented file (REC_FRAGMENT_SECS=0): the finalise must still
    // apply the track layout, by appending a patched moov at the end.
    let tracks = vec![
        TrackFixture {
            id: 1,
            handler: *b"vide",
            timescale: 3000,
            duration: 3000,
            flags: 0x7,
            tables: stbl_with_tables(),
        },
        TrackFixture {
            id: 2,
            handler: *b"soun",
            timescale: 48000,
            duration: 96000,
            flags: 0x7,
            tables: stbl_with_tables(),
        },
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
    let report = finalize(&path, &layout(), CrashAt::None, None, None).unwrap();
    assert!(!report.defragmented);
    let info = read(&path).unwrap();
    assert!(!info.fragmented);
    let audio = info
        .tracks
        .iter()
        .find(|t| t.handler == Handler::Audio)
        .unwrap();
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
    let tracks = vec![TrackFixture {
        id: 1,
        handler: *b"soun",
        timescale: 44100,
        duration: 0,
        flags: 0x7,
        tables: empty_tables(),
    }];
    let path = write_temp("scale", &[ftyp(), init_moov(1000, &tracks)].concat());
    // Two AAC frames at 44.1 kHz: 2 * 1024/48000 s = 42.67 ms worth of media.
    // Put samples into the file through the fragmented path instead: a moof.
    let mut bytes = std::fs::read(&path).unwrap();
    let samples: [S; 10] = [(940, 5, 0, 0); 10];
    let moof = moof_at(
        bytes.len() as u64,
        &[TrafSpec {
            track_id: 1,
            defaults: None,
            samples: &samples,
            data_offset: true,
        }],
    );
    bytes.extend_from_slice(&moof);
    bytes.extend_from_slice(&mdat(&[0u8; 50]));
    std::fs::write(&path, &bytes).unwrap();
    let layout = Layout {
        audio_alternate_group: Some(1),
        default_audio: Some(0),
        audio_priming: vec![2048],
    };
    finalize(&path, &layout, CrashAt::None, None, None).unwrap();
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
    finalize(&path, &layout(), CrashAt::None, Some(&keep), None).unwrap();
    let kept = std::fs::read(&keep).unwrap();
    assert_eq!(kept, bytes, "the kept copy is the fragmented file");
    std::fs::remove_file(&keep).ok();
    std::fs::remove_file(&path).ok();
}

/// qtmux retimes audio buffers it considers irregular and writes the
/// compensation as a *negative* composition offset, in a version-0 trun whose
/// offsets the spec calls unsigned. Reading it as unsigned saturated it to
/// i32::MAX, which put a real microphone's 11th packet at 44739 s after the
/// finalise. Parse it signed and rebuild a uniform timeline from it.
#[test]
fn a_version_0_trun_with_a_negative_composition_offset_parses_signed() {
    // tfhd: default duration 1024; trun v0 with duration/size/flags/cts.
    let mut tfhd_body = 1u32.to_be_bytes().to_vec();
    tfhd_body.extend_from_slice(&1024u32.to_be_bytes());
    let mut traf = full_flags(b"tfhd", 0, 0x8, &tfhd_body);
    traf.extend_from_slice(&tfdt(0));
    let mut trun_body = 2u32.to_be_bytes().to_vec();
    trun_body.extend_from_slice(&332i32.to_be_bytes()); // data_offset
    for (dur, size, cts) in [(1024u32, 21u32, 0i32), (0u32, 4u32, -3991i32)] {
        trun_body.extend_from_slice(&dur.to_be_bytes());
        trun_body.extend_from_slice(&size.to_be_bytes());
        trun_body.extend_from_slice(&0u32.to_be_bytes());
        trun_body.extend_from_slice(&cts.to_be_bytes());
    }
    traf.extend_from_slice(&full_flags(b"trun", 0, 0xf01, &trun_body));
    let mut body = full_flags(b"mfhd", 0, 0, &[0; 4]);
    let mut traf_box = vec![];
    push_box(&mut traf_box, b"traf", &traf);
    body.extend_from_slice(&traf_box);
    let mut moof = vec![];
    push_box(&mut moof, b"moof", &body);
    let out = parse_moof(&moof, 0, &HashMap::new()).unwrap();
    let (_, _t, samples) = &out[0];
    assert_eq!(samples[1].cts, -3991, "signed, not saturated");
    assert_eq!(samples[0].cts, 0);
}

#[test]
fn a_retimed_audio_timeline_is_rebuilt_uniform_and_monotonic() {
    let samples = vec![
        Sample {
            offset: 0,
            size: 21,
            duration: 1024,
            cts: 0,
            sync: true,
        },
        Sample {
            offset: 21,
            size: 4,
            duration: 0,
            cts: -3991,
            sync: true,
        },
        // A negative duration written as a wrapped u32.
        Sample {
            offset: 25,
            size: 4,
            duration: 4_294_963_305,
            cts: -3991,
            sync: true,
        },
        Sample {
            offset: 29,
            size: 4,
            duration: 1024,
            cts: 0,
            sync: true,
        },
    ];
    let fixed = sane_durations(&samples, 48_000);
    assert!(
        fixed.iter().all(|s| s.duration == 1024 && s.cts == 0),
        "{fixed:?}"
    );
    let tables = build_tables(&fixed);
    assert_eq!(tables.stts, vec![(4, 1024)], "one monotonic run");
    assert!(tables.ctts.is_none(), "the compensating cts is dropped");
    assert_eq!(media_duration(&fixed), 4096);
}

/// A video sample on the output grid (60 fps at timescale 6000).
fn video_sample(duration: u32) -> Sample {
    Sample {
        offset: 0,
        size: 1,
        duration,
        cts: 0,
        sync: true,
    }
}

#[test]
fn a_drop_at_the_end_of_the_take_extends_the_last_sample() {
    // 600 slots were due, the encoder refused the last four, so 596 frames
    // were written. The last one must hold to slot 600 or the video track ends
    // 83 ms short of the audio.
    let mut samples: Vec<Sample> = (0..596).map(|_| video_sample(100)).collect();
    extend_last_sample(&mut samples, 600 * 100);
    assert_eq!(samples.last().unwrap().duration, 500);
    assert_eq!(media_duration(&samples), 60_000);
}

#[test]
fn drops_during_the_stop_drain_still_reach_the_end() {
    // The drain at stop lost 30 slots: the last written frame covers its own
    // frame plus those 30.
    let mut samples: Vec<Sample> = (0..570).map(|_| video_sample(100)).collect();
    extend_last_sample(&mut samples, 600 * 100);
    assert_eq!(samples.last().unwrap().duration, 3_100);
    assert_eq!(media_duration(&samples), 60_000);
}

#[test]
fn a_mid_take_gap_is_kept_and_the_tail_still_reaches_the_end() {
    // One mid-take drop (a 300-unit gap) and nine trailing drops. The gap is
    // already in the durations; only the last sample is extended.
    let mut samples: Vec<Sample> = (0..590).map(|_| video_sample(100)).collect();
    samples[100].duration = 300;
    extend_last_sample(&mut samples, 600 * 100);
    assert_eq!(samples[100].duration, 300, "the mid-take gap stays");
    assert_eq!(samples.last().unwrap().duration, 900);
    assert_eq!(media_duration(&samples), 60_000);
}

/// Build a single-track movie with independently timed fragments. Payload
/// bytes identify packets so the assertions can check sample order as well as
/// timing after rewriting the tables.
fn timeline_fixture(
    handler: [u8; 4],
    timescale: u32,
    fragments: &[(&[S], Option<u64>)],
) -> Vec<u8> {
    let tracks = [TrackFixture {
        id: 1,
        handler,
        timescale,
        duration: 0,
        flags: 0x7,
        tables: empty_tables(),
    }];
    let mut bytes = [ftyp(), init_moov(1000, &tracks)].concat();
    let mut packet = 1u8;
    for (samples, time) in fragments {
        let moof = moof_at_times(
            bytes.len() as u64,
            &[TrafSpec {
                track_id: 1,
                defaults: None,
                samples,
                data_offset: true,
            }],
            &[*time],
        );
        bytes.extend_from_slice(&moof);
        let mut payload = vec![];
        for sample in *samples {
            payload.extend(std::iter::repeat_n(packet, sample.1 as usize));
            packet += 1;
        }
        bytes.extend_from_slice(&mdat(&payload));
    }
    bytes
}

/// Inspect the *serialized* regular sample tables, independently of
/// build_tables: each tuple is (DTS, PTS, duration). This catches a correct
/// aggregate duration that nevertheless shifts packets within the take.
fn regular_packet_times(path: &Path) -> Vec<(i64, i64, u32)> {
    fn descendant<'a>(buf: &'a [u8], kinds: &[[u8; 4]]) -> &'a [u8] {
        let Some((kind, rest)) = kinds.split_first() else {
            return buf;
        };
        let kids = children(buf, 8, buf.len()).unwrap();
        descendant(slice(buf, find(&kids, kind).unwrap()), rest)
    }
    let (mut file, boxes, _) = scan(path).unwrap();
    let moov = boxes.iter().rfind(|b| b.kind == MOOV).unwrap();
    let moov = read_at(&mut file, moov.start, moov.size).unwrap();
    let stbl = descendant(&moov, &[*b"trak", *b"mdia", *b"minf", *b"stbl"]);
    let kids = children(stbl, 8, stbl.len()).unwrap();
    let stts = slice(stbl, find(&kids, b"stts").unwrap());
    let mut durations = vec![];
    for i in 0..be32(stts, 12).unwrap() as usize {
        let count = be32(stts, 16 + i * 8).unwrap();
        let delta = be32(stts, 20 + i * 8).unwrap();
        durations.extend(std::iter::repeat_n(delta, count as usize));
    }
    let mut offsets = vec![];
    if let Some(ctts) = find(&kids, b"ctts") {
        let ctts = slice(stbl, ctts);
        for i in 0..be32(ctts, 12).unwrap() as usize {
            let count = be32(ctts, 16 + i * 8).unwrap();
            let offset = be32(ctts, 20 + i * 8).unwrap();
            let offset = if ctts[8] == 1 {
                offset as i32 as i64
            } else {
                offset as i64
            };
            offsets.extend(std::iter::repeat_n(offset, count as usize));
        }
    } else {
        offsets.resize(durations.len(), 0);
    }
    assert_eq!(offsets.len(), durations.len());
    let mut dts = 0;
    durations
        .into_iter()
        .zip(offsets)
        .map(|(duration, cts)| {
            let packet = (dts, dts + cts, duration);
            dts += duration as i64;
            packet
        })
        .collect()
}

#[test]
fn finalization_retains_long_video_holds_fragment_gaps_and_packet_pts() {
    let first = [
        (100, 1, opt_flags(true), 0),
        // A legitimate three-second mid-take hold, not corrupt audio.
        (18_100, 1, opt_flags(false), 20),
        (100, 1, opt_flags(false), -10),
    ];
    let second = [(100, 1, opt_flags(true), 30), (100, 1, opt_flags(false), 0)];
    let bytes = timeline_fixture(
        *b"vide",
        6000,
        &[
            (&first, Some(0)),
            // The trun ended at 18300: a further 300-unit boundary gap.
            (&second, Some(18_600)),
        ],
    );
    let path = write_temp("video-packet-timeline", &bytes);
    let before = read(&path).unwrap();
    assert_eq!(before.tracks[0].duration, 18_800);
    assert_eq!(before.tracks[0].samples, 5);
    assert_eq!(before.tracks[0].nominal_fps, Some(60.0));
    finalize(
        &path,
        &Layout::default(),
        CrashAt::None,
        None,
        Some(20_000.0 / 6000.0),
    )
    .unwrap();
    assert_eq!(
        regular_packet_times(&path),
        vec![
            (0, 0, 100),
            (100, 120, 18_100),
            (18_200, 18_190, 400),
            (18_600, 18_630, 100),
            (18_700, 18_700, 1300),
        ]
    );
    let after = read(&path).unwrap();
    assert_eq!(after.tracks[0].duration, 20_000);
    assert_eq!(after.duration, 3333);
    assert_eq!(after.tracks[0].samples, 5);
    assert_eq!(after.tracks[0].bytes, 5);
    // No media is moved or replaced when its timing is changed.
    let finalized = std::fs::read(&path).unwrap();
    let (_, boxes, _) = scan(&path).unwrap();
    for b in boxes.iter().filter(|b| &b.kind == b"mdat") {
        let range = b.body_start() as usize..(b.start + b.size) as usize;
        assert_eq!(&finalized[range.clone()], &bytes[range]);
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_boundary_gap_is_not_added_twice_when_the_trun_already_covers_it() {
    let first = [(100, 1, 0, 0), (12_000, 1, 0, 0)];
    let second = [(100, 1, 0, 0), (100, 1, 0, 0)];
    let bytes = timeline_fixture(
        *b"vide",
        6000,
        &[(&first, Some(0)), (&second, Some(12_100))],
    );
    let path = write_temp("boundary-already-covered", &bytes);
    assert_eq!(read(&path).unwrap().tracks[0].duration, 12_300);
    finalize(&path, &Layout::default(), CrashAt::None, None, None).unwrap();
    assert_eq!(
        regular_packet_times(&path),
        vec![
            (0, 0, 100),
            (100, 100, 12_000),
            (12_100, 12_100, 100),
            (12_200, 12_200, 100),
        ]
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn missing_tfdt_continues_video_after_an_explicit_64_bit_origin() {
    let samples = [(100, 1, 0, 0), (100, 1, 0, 0)];
    let origin = (1u64 << 32) + 100;
    let bytes = timeline_fixture(
        *b"vide",
        6000,
        &[
            (&samples, Some(origin)),
            (&samples, None),
            (&samples, Some(origin + 700)),
        ],
    );
    let path = write_temp("missing-tfdt", &bytes);
    let info = read(&path).unwrap();
    assert_eq!(info.tracks[0].first_pts, origin as f64 / 6000.0);
    assert_eq!(info.tracks[0].duration, 900);
    finalize(&path, &Layout::default(), CrashAt::None, None, None).unwrap();
    assert_eq!(
        regular_packet_times(&path),
        vec![
            (0, 0, 100),
            (100, 100, 100),
            (200, 200, 100),
            (300, 300, 400),
            (700, 700, 100),
            (800, 800, 100),
        ]
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn separate_trafs_for_one_track_keep_their_decode_time_boundaries() {
    let samples = [(100, 1, 0, 0), (100, 1, 0, 0)];
    let tracks = [TrackFixture {
        id: 1,
        handler: *b"vide",
        timescale: 6000,
        duration: 0,
        flags: 0x7,
        tables: empty_tables(),
    }];
    let mut bytes = [ftyp(), init_moov(1000, &tracks)].concat();
    let moof = moof_at_times(
        bytes.len() as u64,
        &[
            TrafSpec {
                track_id: 1,
                defaults: None,
                samples: &samples,
                data_offset: true,
            },
            TrafSpec {
                track_id: 1,
                defaults: None,
                samples: &samples,
                data_offset: false,
            },
        ],
        &[Some(0), Some(500)],
    );
    bytes.extend_from_slice(&moof);
    bytes.extend_from_slice(&mdat(&[1, 2, 3, 4]));
    let path = write_temp("same-track-trafs", &bytes);
    assert_eq!(read(&path).unwrap().tracks[0].duration, 700);
    finalize(&path, &Layout::default(), CrashAt::None, None, None).unwrap();
    assert_eq!(
        regular_packet_times(&path),
        vec![
            (0, 0, 100),
            (100, 100, 400),
            (500, 500, 100),
            (600, 600, 100),
        ]
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn zero_video_duration_does_not_trigger_audio_uniformization() {
    let samples = [(100, 1, 0, 10), (0, 1, 0, -10), (100, 1, 0, 20)];
    let bytes = timeline_fixture(*b"vide", 6000, &[(&samples, Some(0))]);
    let path = write_temp("video-zero-duration", &bytes);
    assert_eq!(read(&path).unwrap().tracks[0].duration, 200);
    finalize(
        &path,
        &Layout::default(),
        CrashAt::None,
        None,
        Some(150.0 / 6000.0),
    )
    .unwrap();
    assert_eq!(
        regular_packet_times(&path),
        vec![(0, 10, 100), (100, 90, 0), (100, 120, 100),]
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn overlapping_video_fragments_probe_but_cannot_be_finalized_losslessly() {
    let samples = [(100, 1, 0, 0), (100, 1, 0, 0)];
    let bytes = timeline_fixture(
        *b"vide",
        6000,
        &[(&samples, Some(0)), (&samples, Some(100))],
    );
    let path = write_temp("overlapping-tfdt", &bytes);
    let info = read(&path).unwrap();
    assert!(info.fragmented);
    assert_eq!(info.tracks[0].duration, 300);
    assert_eq!(info.tracks[0].samples, 4);
    assert_eq!(info.tracks[0].bytes, 4);
    assert_eq!(info.tracks[0].first_pts, 0.0);
    assert_eq!(info.tracks[0].nominal_fps, Some(60.0));
    assert!(
        finalize(&path, &Layout::default(), CrashAt::None, None, None)
            .unwrap_err()
            .to_string()
            .contains("overlapping")
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn fragmented_probe_accepts_one_tick_server_timestamp_rounding_overlap() {
    // MediaMTX's WHIP/RTMP recordings can round tfdt one tick below the
    // preceding trun's end (e.g. 14999 versus 15000 at 90 kHz).
    let samples = [(3000, 1, 0, 0); 5];
    let bytes = timeline_fixture(
        *b"vide",
        90_000,
        &[(&samples, Some(0)), (&samples, Some(14_999))],
    );
    let path = write_temp("probe-server-rounding", &bytes);
    let info = read(&path).unwrap();
    assert_eq!(info.tracks[0].duration, 29_999);
    assert_eq!(info.tracks[0].samples, 10);
    assert_eq!(info.tracks[0].bytes, 10);
    assert_eq!(info.tracks[0].nominal_fps, Some(30.0));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn fragmented_probe_uses_decode_extent_and_continues_the_previous_fragment() {
    let long = [(1000, 1, 0, 0)];
    let short = [(100, 1, 0, 0)];
    let origin = (1u64 << 32) + 6000;
    let bytes = timeline_fixture(
        *b"vide",
        6000,
        &[
            (&long, Some(origin)),
            (&short, Some(origin - 100)),
            // Continue at origin, not at the maximum end (origin + 1000).
            (&short, None),
        ],
    );
    let path = write_temp("probe-backwards-fragments", &bytes);
    let info = read(&path).unwrap();
    assert_eq!(info.tracks[0].duration, 1100);
    assert_eq!(info.tracks[0].first_pts, (origin - 100) as f64 / 6000.0);
    assert_eq!(info.tracks[0].samples, 3);
    assert_eq!(info.tracks[0].bytes, 3);
    assert_eq!(info.tracks[0].nominal_fps, Some(60.0));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn audio_repair_still_applies_when_reading_and_finalizing_fragments() {
    let samples = [
        (1024, 1, 0, 0),
        (0, 1, 0, -3991),
        (4_294_963_305, 1, 0, -3991),
        (1024, 1, 0, 0),
    ];
    let bytes = timeline_fixture(*b"soun", 48_000, &[(&samples, Some(0))]);
    let path = write_temp("audio-duration-repair", &bytes);
    assert_eq!(read(&path).unwrap().tracks[0].duration, 4096);
    finalize(&path, &Layout::default(), CrashAt::None, None, None).unwrap();
    assert_eq!(
        regular_packet_times(&path),
        vec![
            (0, 0, 1024),
            (1024, 1024, 1024),
            (2048, 2048, 1024),
            (3072, 3072, 1024),
        ]
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn an_end_inside_the_last_sample_does_not_shorten_its_existing_hold() {
    let mut samples = vec![video_sample(100), video_sample(1200)];
    extend_last_sample(&mut samples, 500);
    assert_eq!(samples[1].duration, 1200);
    assert_eq!(media_duration(&samples), 1300);
}

#[test]
fn an_end_before_the_last_sample_does_not_shorten_it() {
    let mut samples: Vec<Sample> = (0..596).map(|_| video_sample(100)).collect();
    extend_last_sample(&mut samples, 500 * 100);
    assert_eq!(samples.last().unwrap().duration, 100);
    assert_eq!(media_duration(&samples), 59_600);
}
