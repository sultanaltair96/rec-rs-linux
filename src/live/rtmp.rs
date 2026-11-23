//! RTMP / RTMPS publishing (YouTube, Twitch, Kick, any RTMP server):
//! handshake, chunk stream, AMF0 commands and FLV tags for H.264 + AAC (and
//! enhanced RTMP's `hvc1` for HEVC), with a bounded send queue, GOP dropping
//! under congestion and reconnects with backoff.
//!
//! TLS is `rustls` (the ring provider, so no system OpenSSL is needed); roots
//! come from `webpki-roots`. `REC_INSECURE_TLS=1` accepts a self-signed server
//! certificate, which is how the self-test talks to a local MediaMTX over
//! RTMPS.
//!
//! Adapted from rec-rs src/live/rtmp.rs (MIT OR Apache-2.0): native-tls became
//! rustls and the CoreMedia format description became the hub's [`Formats`].

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::audio::{AAC_ASC, AudioCodec};
use super::{Formats, Output, OutputStatus};
use crate::media_types::{EncodedPacket, PacketKind};

// ---------------------------------------------------------------- AMF0

#[derive(Clone, Debug, PartialEq)]
pub enum Amf {
    Number(f64),
    Bool(bool),
    Str(String),
    Object(Vec<(String, Amf)>),
    Null,
    Undefined,
    Ecma(Vec<(String, Amf)>),
    Array(Vec<Amf>),
}

impl Amf {
    pub fn encode(&self, out: &mut Vec<u8>) {
        let props = |out: &mut Vec<u8>, kv: &[(String, Amf)]| {
            for (k, v) in kv {
                out.extend_from_slice(&(k.len() as u16).to_be_bytes());
                out.extend_from_slice(k.as_bytes());
                v.encode(out);
            }
            out.extend_from_slice(&[0, 0, 9]);
        };
        match self {
            Amf::Number(n) => {
                out.push(0);
                out.extend_from_slice(&n.to_be_bytes());
            }
            Amf::Bool(b) => out.extend_from_slice(&[1, *b as u8]),
            Amf::Str(s) => {
                out.push(2);
                out.extend_from_slice(&(s.len() as u16).to_be_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            Amf::Object(kv) => {
                out.push(3);
                props(out, kv);
            }
            Amf::Null => out.push(5),
            Amf::Undefined => out.push(6),
            Amf::Ecma(kv) => {
                out.push(8);
                out.extend_from_slice(&(kv.len() as u32).to_be_bytes());
                props(out, kv);
            }
            Amf::Array(items) => {
                out.push(0x0A);
                out.extend_from_slice(&(items.len() as u32).to_be_bytes());
                for v in items {
                    v.encode(out);
                }
            }
        }
    }

    /// Decode one value; returns it and the bytes used.
    pub fn decode(b: &[u8]) -> Option<(Amf, usize)> {
        let props = |b: &[u8]| -> Option<(Vec<(String, Amf)>, usize)> {
            let mut i = 0;
            let mut kv = vec![];
            loop {
                let n = u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as usize;
                i += 2;
                if n == 0 && *b.get(i)? == 9 {
                    return Some((kv, i + 1));
                }
                let k = String::from_utf8_lossy(b.get(i..i + n)?).into_owned();
                i += n;
                let (v, used) = Amf::decode(&b[i..])?;
                i += used;
                kv.push((k, v));
            }
        };
        match *b.first()? {
            0 => Some((
                Amf::Number(f64::from_be_bytes(b.get(1..9)?.try_into().ok()?)),
                9,
            )),
            1 => Some((Amf::Bool(*b.get(1)? != 0), 2)),
            2 => {
                let n = u16::from_be_bytes([*b.get(1)?, *b.get(2)?]) as usize;
                Some((
                    Amf::Str(String::from_utf8_lossy(b.get(3..3 + n)?).into_owned()),
                    3 + n,
                ))
            }
            3 => props(&b[1..]).map(|(kv, n)| (Amf::Object(kv), 1 + n)),
            5 => Some((Amf::Null, 1)),
            6 => Some((Amf::Undefined, 1)),
            8 => props(&b[5..]).map(|(kv, n)| (Amf::Ecma(kv), 5 + n)),
            0x0A => {
                let n = u32::from_be_bytes(b.get(1..5)?.try_into().ok()?) as usize;
                let (mut items, mut i) = (vec![], 5);
                for _ in 0..n {
                    let (v, used) = Amf::decode(b.get(i..)?)?;
                    items.push(v);
                    i += used;
                }
                Some((Amf::Array(items), i))
            }
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Amf> {
        match self {
            Amf::Object(kv) | Amf::Ecma(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&str> {
        if let Amf::Str(s) = self {
            Some(s)
        } else {
            None
        }
    }
}

fn amf_all(b: &[u8]) -> Vec<Amf> {
    let mut v = vec![];
    let mut i = 0;
    while i < b.len() {
        let Some((a, n)) = Amf::decode(&b[i..]) else {
            break;
        };
        v.push(a);
        i += n;
    }
    v
}

fn obj(kv: &[(&str, Amf)]) -> Amf {
    Amf::Object(kv.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
}

fn command(values: &[Amf]) -> Vec<u8> {
    let mut out = vec![];
    for v in values {
        v.encode(&mut out);
    }
    out
}

// ---------------------------------------------------------------- URLs

#[derive(Clone, Debug, PartialEq)]
pub struct RtmpUrl {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub app: String,
    pub tc_url: String,
}

pub fn parse_url(url: &str) -> Result<RtmpUrl> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("rtmps://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("rtmp://") {
        (false, r)
    } else {
        bail!("not an rtmp:// or rtmps:// URL");
    };
    let (hostport, app) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().context("bad port")?),
        None => (hostport.to_string(), if tls { 443 } else { 1935 }),
    };
    if host.is_empty() {
        bail!("missing host");
    }
    let app = app.trim_end_matches('/').to_string();
    let scheme = if tls { "rtmps" } else { "rtmp" };
    Ok(RtmpUrl {
        tls,
        tc_url: format!("{scheme}://{hostport}/{app}"),
        host,
        port,
        app,
    })
}

// ---------------------------------------------------------------- chunks

const C_CONTROL: u8 = 2;
const C_COMMAND: u8 = 3;
const C_AUDIO: u8 = 4;
const C_VIDEO: u8 = 6;
const OUT_CHUNK: usize = 4096;

/// One RTMP message split into chunks (type-0 header, then type-3).
pub fn chunk_message(
    csid: u8,
    msg_type: u8,
    stream_id: u32,
    ts: u32,
    payload: &[u8],
    chunk_size: usize,
) -> Vec<u8> {
    let ext = ts >= 0xFF_FFFF;
    let mut out = Vec::with_capacity(payload.len() + 16 + payload.len() / chunk_size * 5);
    out.push(csid & 0x3f);
    let t = if ext { 0xFF_FFFF } else { ts };
    out.extend_from_slice(&t.to_be_bytes()[1..]);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
    out.push(msg_type);
    out.extend_from_slice(&stream_id.to_le_bytes());
    if ext {
        out.extend_from_slice(&ts.to_be_bytes());
    }
    for (i, part) in payload.chunks(chunk_size.max(1)).enumerate() {
        if i > 0 {
            out.push(0xC0 | (csid & 0x3f));
            if ext {
                out.extend_from_slice(&ts.to_be_bytes());
            }
        }
        out.extend_from_slice(part);
    }
    if payload.is_empty() {
        return out;
    }
    out
}

#[derive(Default, Clone)]
struct InState {
    ts: u32,
    len: usize,
    typ: u8,
    sid: u32,
    buf: Vec<u8>,
}

/// Incoming chunk stream parser.
pub struct ChunkReader {
    chunk_size: usize,
    states: HashMap<u32, InState>,
    data: Vec<u8>,
}

/// A complete incoming message.
#[derive(Debug, PartialEq)]
pub struct Message {
    pub typ: u8,
    pub sid: u32,
    pub payload: Vec<u8>,
}

impl Default for ChunkReader {
    fn default() -> Self {
        ChunkReader {
            chunk_size: 128,
            states: HashMap::new(),
            data: vec![],
        }
    }
}

impl ChunkReader {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Message> {
        self.data.extend_from_slice(bytes);
        let mut out = vec![];
        while let Some((used, msg)) = self.one() {
            self.data.drain(..used);
            if let Some(m) = msg {
                if m.typ == 1 && m.payload.len() >= 4 {
                    self.chunk_size = (u32::from_be_bytes(m.payload[..4].try_into().unwrap())
                        & 0x7fff_ffff) as usize;
                }
                out.push(m);
            }
        }
        out
    }

    /// Parse one chunk from the buffer: (bytes used, finished message).
    fn one(&mut self) -> Option<(usize, Option<Message>)> {
        let d = &self.data;
        let b0 = *d.first()?;
        let fmt = b0 >> 6;
        let (csid, mut i) = match b0 & 0x3f {
            0 => (*d.get(1)? as u32 + 64, 2),
            1 => (*d.get(1)? as u32 + 64 + *d.get(2)? as u32 * 256, 3),
            c => (c as u32, 1),
        };
        let mut st = self.states.get(&csid).cloned().unwrap_or_default();
        let hdr = [11, 7, 3, 0][fmt as usize];
        let h = d.get(i..i + hdr)?;
        let be3 = |b: &[u8]| u32::from_be_bytes([0, b[0], b[1], b[2]]);
        let mut ts_field = None;
        if fmt <= 2 {
            ts_field = Some(be3(&h[0..3]));
        }
        if fmt <= 1 {
            st.len = be3(&h[3..6]) as usize;
            st.typ = h[6];
        }
        if fmt == 0 {
            st.sid = u32::from_le_bytes(h[7..11].try_into().unwrap());
        }
        i += hdr;
        if ts_field == Some(0xFF_FFFF) {
            ts_field = Some(u32::from_be_bytes(d.get(i..i + 4)?.try_into().unwrap()));
            i += 4;
        }
        if let Some(t) = ts_field {
            st.ts = if fmt == 0 { t } else { st.ts.wrapping_add(t) };
        }
        let want = (st.len - st.buf.len()).min(self.chunk_size.max(1));
        let part = d.get(i..i + want)?;
        st.buf.extend_from_slice(part);
        i += want;
        let msg = (st.buf.len() >= st.len).then(|| Message {
            typ: st.typ,
            sid: st.sid,
            payload: std::mem::take(&mut st.buf),
        });
        self.states.insert(csid, st);
        Some((i, msg))
    }
}

// ---------------------------------------------------------------- FLV

/// FourCC of HEVC in enhanced RTMP (E-RTMP v1).
const HVC1: &[u8; 4] = b"hvc1";

/// Video sequence header: legacy AVC (codec id 7) with the `avcC` record, or an
/// enhanced-RTMP `hvc1` SequenceStart with the `hvcC` record.
pub fn flv_video_header(hevc: bool, record: &[u8]) -> Vec<u8> {
    let mut v = if hevc {
        [&[0x80 | 0x10][..], HVC1].concat()
    } else {
        vec![0x17, 0, 0, 0, 0]
    };
    v.extend_from_slice(record);
    v
}

/// One access unit (4-byte length-prefixed NAL units). HEVC uses enhanced
/// RTMP's CodedFramesX (no composition offset: no B-frames).
pub fn flv_video(hevc: bool, key: bool, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(data.len() + 5);
    if hevc {
        v.push(0x80 | if key { 0x10 } else { 0x20 } | 3);
        v.extend_from_slice(HVC1);
    } else {
        v.extend_from_slice(&[if key { 0x17 } else { 0x27 }, 1, 0, 0, 0]);
    }
    v.extend_from_slice(data);
    v
}

pub fn flv_audio_header() -> Vec<u8> {
    let mut v = vec![0xAF, 0];
    v.extend_from_slice(&AAC_ASC);
    v
}

pub fn flv_audio(raw: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(raw.len() + 2);
    v.extend_from_slice(&[0xAF, 1]);
    v.extend_from_slice(raw);
    v
}

// ---------------------------------------------------------------- connection

trait Stream: Read + Write + Send {
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()>;
}

impl Stream for TcpStream {
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_read_timeout(self, d)
    }
}

/// A TLS stream that can still change its socket timeout (rustls has no such
/// call on the connection).
struct TlsStream {
    inner: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    socket: TcpStream,
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl Stream for TlsStream {
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        self.socket.set_read_timeout(d)
    }
}

/// Certificate verifier that accepts anything: only for `REC_INSECURE_TLS`,
/// i.e. a local test server with a self-signed certificate.
#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// The TLS client configuration: the webpki roots, or — for a **loopback** host
/// and `REC_INSECURE_TLS`, the self-test's local server — no verification at all.
fn tls_config(host: &str) -> Result<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow!("TLS: {e}"))?;
    let config = if super::ignore_certificate_errors(host) {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
    } else {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        builder.with_root_certificates(roots)
    };
    Ok(Arc::new(config.with_no_client_auth()))
}

fn tls_connect(host: &str, tcp: TcpStream) -> Result<TlsStream> {
    let config = tls_config(host)?;
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| anyhow!("TLS: bad server name {host:?}: {e}"))?;
    let conn = rustls::ClientConnection::new(config, name).map_err(|e| anyhow!("TLS: {e}"))?;
    let socket = tcp.try_clone()?;
    Ok(TlsStream {
        inner: rustls::StreamOwned::new(conn, tcp),
        socket,
    })
}

struct Conn {
    s: Box<dyn Stream>,
    reader: ChunkReader,
    stream_id: u32,
    sent: u64,
    /// The server didn't rule out enhanced-RTMP HEVC.
    hevc_ok: bool,
}

impl Conn {
    fn open(u: &RtmpUrl, key: &str) -> Result<Conn> {
        let debug = std::env::var_os("REC_DEBUG").is_some();
        let addr = format!("{}:{}", u.host, u.port);
        if debug {
            eprintln!("rec: debug: rtmp connecting to {addr}");
        }
        let tcp = std::net::ToSocketAddrs::to_socket_addrs(&addr)?
            .find_map(|a| TcpStream::connect_timeout(&a, Duration::from_secs(10)).ok())
            .ok_or_else(|| anyhow!("cannot connect to {addr}"))?;
        tcp.set_nodelay(true)?;
        tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
        let s: Box<dyn Stream> = if u.tls {
            Box::new(tls_connect(&u.host, tcp)?)
        } else {
            Box::new(tcp)
        };
        let mut c = Conn {
            s,
            reader: ChunkReader::default(),
            stream_id: 0,
            sent: 0,
            hevc_ok: true,
        };
        c.s.set_read_timeout(Some(Duration::from_secs(10)))?;
        if debug {
            eprintln!("rec: debug: rtmp connected, handshake");
        }
        c.handshake()?;
        if debug {
            eprintln!("rec: debug: rtmp handshake done, publish");
        }
        c.publish(u, key)?;
        Ok(c)
    }

    fn handshake(&mut self) -> Result<()> {
        let debug = std::env::var_os("REC_DEBUG").is_some();
        let mut c1 = vec![0u8; 1536];
        // The handshake only wants unpredictable bytes; read exactly the
        // buffer (a plain read of /dev/urandom never ends) and fall back to
        // zeros, which servers accept.
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            use std::io::Read as _;
            let _ = f.read_exact(&mut c1[8..]);
        }
        c1[..8].fill(0);
        let mut c0c1 = vec![3u8];
        c0c1.extend_from_slice(&c1);
        self.s.write_all(&c0c1)?;
        if debug {
            eprintln!("rec: debug: rtmp C0+C1 sent");
        }
        let mut s0s1 = vec![0u8; 1537];
        self.s.read_exact(&mut s0s1).context("handshake (S0/S1)")?;
        if debug {
            eprintln!("rec: debug: rtmp S0+S1 received");
        }
        if s0s1[0] != 3 {
            bail!("unexpected RTMP version {}", s0s1[0]);
        }
        self.s.write_all(&s0s1[1..])?; // C2 echoes S1
        if debug {
            eprintln!("rec: debug: rtmp C2 sent");
        }
        let mut s2 = vec![0u8; 1536];
        self.s.read_exact(&mut s2).context("handshake (S2)")?;
        if debug {
            eprintln!("rec: debug: rtmp S2 received");
        }
        Ok(())
    }

    fn send(&mut self, csid: u8, typ: u8, sid: u32, ts: u32, payload: &[u8]) -> Result<()> {
        let bytes = chunk_message(csid, typ, sid, ts, payload, OUT_CHUNK);
        self.s.write_all(&bytes)?;
        self.sent += bytes.len() as u64;
        Ok(())
    }

    fn cmd(&mut self, sid: u32, values: &[Amf]) -> Result<()> {
        if std::env::var_os("REC_DEBUG").is_some() {
            let name = values.first().and_then(Amf::str).unwrap_or("?");
            eprintln!("rec: debug: rtmp -> {name} (stream {sid})");
        }
        self.send(C_COMMAND, 20, sid, 0, &command(values))
    }

    /// Read messages until `pred` accepts a command (or an error arrives).
    fn wait_command(
        &mut self,
        what: &str,
        mut pred: impl FnMut(&[Amf]) -> bool,
    ) -> Result<Vec<Amf>> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut buf = [0u8; 8192];
        loop {
            if Instant::now() > deadline {
                bail!("timed out waiting for {what}");
            }
            let n = self
                .s
                .read(&mut buf)
                .with_context(|| format!("waiting for {what}"))?;
            if n == 0 {
                bail!("server closed the connection while waiting for {what}");
            }
            for m in self.reader.feed(&buf[..n]) {
                if m.typ == 4
                    && m.payload.len() >= 6
                    && u16::from_be_bytes([m.payload[0], m.payload[1]]) == 6
                {
                    // Ping request: answer with a pong.
                    let mut p = vec![0, 7];
                    p.extend_from_slice(&m.payload[2..6]);
                    self.send(C_CONTROL, 4, 0, 0, &p)?;
                }
                if m.typ == 20 {
                    let v = amf_all(&m.payload);
                    let name = v.first().and_then(Amf::str).unwrap_or("");
                    if name == "_error" || (name == "onStatus" && status_level(&v) == Some("error"))
                    {
                        bail!("{what} refused: {}", status_text(&v));
                    }
                    if std::env::var_os("REC_DEBUG").is_some() {
                        eprintln!(
                            "rec: debug: rtmp <- {} {}",
                            v.first().and_then(Amf::str).unwrap_or("?"),
                            status_text(&v)
                        );
                    }
                    if pred(&v) {
                        return Ok(v);
                    }
                }
            }
        }
    }

    fn publish(&mut self, u: &RtmpUrl, key: &str) -> Result<()> {
        // Larger outgoing chunks cut header overhead.
        self.send(C_CONTROL, 1, 0, 0, &(OUT_CHUNK as u32).to_be_bytes())?;
        self.cmd(
            0,
            &[
                Amf::Str("connect".into()),
                Amf::Number(1.0),
                obj(&[
                    ("app", Amf::Str(u.app.clone())),
                    ("type", Amf::Str("nonprivate".into())),
                    ("flashVer", Amf::Str("FMLE/3.0 (compatible; rec-rs)".into())),
                    ("tcUrl", Amf::Str(u.tc_url.clone())),
                    // Enhanced RTMP: codecs we can send.
                    (
                        "fourCcList",
                        Amf::Array(vec![Amf::Str("hvc1".into()), Amf::Str("avc1".into())]),
                    ),
                ]),
            ],
        )?;
        let r = self.wait_command("connect", |v| {
            v.first().and_then(Amf::str) == Some("_result") && v.get(1) == Some(&Amf::Number(1.0))
        })?;
        self.hevc_ok = hevc_allowed(&r);
        self.cmd(
            0,
            &[
                Amf::Str("releaseStream".into()),
                Amf::Number(2.0),
                Amf::Null,
                Amf::Str(key.into()),
            ],
        )?;
        self.cmd(
            0,
            &[
                Amf::Str("FCPublish".into()),
                Amf::Number(3.0),
                Amf::Null,
                Amf::Str(key.into()),
            ],
        )?;
        self.cmd(
            0,
            &[Amf::Str("createStream".into()), Amf::Number(4.0), Amf::Null],
        )?;
        let r = self.wait_command("createStream", |v| {
            v.first().and_then(Amf::str) == Some("_result") && v.get(1) == Some(&Amf::Number(4.0))
        })?;
        self.stream_id = match r.get(3) {
            Some(Amf::Number(n)) => *n as u32,
            _ => 1,
        };
        let sid = self.stream_id;
        self.cmd(
            sid,
            &[
                Amf::Str("publish".into()),
                Amf::Number(5.0),
                Amf::Null,
                Amf::Str(key.into()),
                Amf::Str("live".into()),
            ],
        )?;
        self.wait_command("publish", |v| {
            v.first().and_then(Amf::str) == Some("onStatus")
                && status_code(v).is_some_and(|c| c.contains("Publish.Start"))
        })?;
        Ok(())
    }

    /// Answer pings and notice a closed connection (non-blocking).
    fn poll(&mut self) -> Result<()> {
        self.s.set_read_timeout(Some(Duration::from_millis(1)))?;
        let mut buf = [0u8; 4096];
        let r = self.s.read(&mut buf);
        self.s.set_read_timeout(Some(Duration::from_secs(10)))?;
        match r {
            Ok(0) => bail!("server closed the connection"),
            Ok(n) => {
                for m in self.reader.feed(&buf[..n]) {
                    if m.typ == 4
                        && m.payload.len() >= 6
                        && u16::from_be_bytes([m.payload[0], m.payload[1]]) == 6
                    {
                        let mut p = vec![0, 7];
                        p.extend_from_slice(&m.payload[2..6]);
                        self.send(C_CONTROL, 4, 0, 0, &p)?;
                    }
                    if m.typ == 20 {
                        let v = amf_all(&m.payload);
                        if status_level(&v) == Some("error") {
                            bail!("server: {}", status_text(&v));
                        }
                    }
                }
                Ok(())
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }
}

/// Whether a server's `connect` result allows enhanced-RTMP HEVC: a server that
/// lists its codecs without `hvc1` (or `*`) cannot take it; one that lists none
/// may (YouTube does).
pub fn hevc_allowed(connect_result: &[Amf]) -> bool {
    !connect_result.iter().any(|a| {
        matches!(a.get("fourCcList"), Some(Amf::Array(l)) if !l.iter().any(|c| matches!(c.str(), Some("hvc1" | "*"))))
    })
}

fn status_obj(v: &[Amf]) -> Option<&Amf> {
    v.iter().skip(2).find(|a| matches!(a, Amf::Object(_)))
}

fn status_level(v: &[Amf]) -> Option<&str> {
    status_obj(v)?.get("level")?.str()
}

fn status_code(v: &[Amf]) -> Option<&str> {
    status_obj(v)?.get("code")?.str()
}

fn status_text(v: &[Amf]) -> String {
    let o = status_obj(v);
    let code = o
        .and_then(|o| o.get("code"))
        .and_then(Amf::str)
        .unwrap_or("");
    let desc = o
        .and_then(|o| o.get("description"))
        .and_then(Amf::str)
        .unwrap_or("");
    format!("{code} {desc}").trim().to_string()
}

// ---------------------------------------------------------------- output

#[derive(Clone, Debug)]
struct Tag {
    typ: u8, // 8 audio, 9 video, 18 data
    ts: u32,
    body: Arc<Vec<u8>>,
}

#[derive(Default)]
struct Shared {
    queue: VecDeque<Tag>,
    /// (HEVC, decoder configuration record) of the video being sent.
    formats: Option<(bool, Vec<u8>)>,
    /// HEVC doesn't work with this server: ask the encoder for H.264.
    hevc_refused: bool,
    /// HEVC sessions that failed early (a server without enhanced RTMP drops
    /// the connection).
    hevc_failures: u32,
    width_height: (usize, usize),
    state: String,
    error: Option<String>,
    live_since: Option<Instant>,
    started: Option<Instant>,
    dropped: u64,
    reconnects: u64,
    /// (time, total bytes sent) samples for the bitrate.
    samples: VecDeque<(Instant, u64)>,
    want_key: bool,
    /// Drop video until the next keyframe (after congestion or reconnect).
    skip_to_key: bool,
    /// The first keyframe, held until an audio packet next to it arrives.
    ///
    /// A server's recorder writes one timestamp order across both tracks, so
    /// whatever it receives first fixes the stream's start; audio that turns up
    /// older than that is discarded ("received too late", which MediaMTX logs)
    /// and the recording's sound ends up shifted. The audio encoder lags the
    /// video one at start, so the session begins on a keyframe **with an audio
    /// packet within one AAC frame of it** (the same rule as WHIP): the two
    /// tracks start together and nothing is older than the start.
    pending_key: Option<(f64, Tag)>,
    /// When the held keyframe arrived, so a stream without audio still starts.
    pending_key_at: Option<Instant>,
    /// Video that arrived after the held keyframe, and audio from the same
    /// wait. They are queued, never dropped: a frame after the first keyframe
    /// belongs to the stream, and dropping it would corrupt the GOP it is in.
    pending_after: Vec<(f64, Tag)>,
    pending_audio: Vec<(f64, Tag)>,
    /// Last timestamp sent per tag type (8 audio, 9 video): a server discards
    /// a sample older than the track's position ("received too late"), so a
    /// tag that would go back in time is dropped, not written.
    last_ts: [Option<u32>; 2],
    /// Newest audio timestamp queued (not yet sent), used to keep the picture
    /// from running ahead of the sound.
    ///
    /// A server's recorder writes samples in one timestamp order across both
    /// tracks, so audio older than the video it already wrote is dropped
    /// ("received too late", which MediaMTX logs). The two encoders are
    /// separate pipelines and the audio one lags under load: video is dropped
    /// rather than sent when the picture is more than `MAX_AV_SKEW_MS` ahead of
    /// it, so the tracks stay in step at the server. Held, never dropped: an
    /// encoded H.264 frame is only useful with the frames after it, so the only
    /// thing this output ever sheds is a whole GOP (the congestion rule).
    newest_audio: Option<u32>,
    /// When the newest audio tag arrived, so a stalled audio encoder cannot
    /// hold the picture back forever.
    newest_audio_at: Option<Instant>,
    /// Host time of the stream's start (the first audio packet next to the
    /// first keyframe), set once per connection by `maybe_start`.
    t0: Option<f64>,
    /// First audio time seen before the stream started, and the newest audio
    /// time seen, for the start decision.
    first_audio_pts: Option<f64>,
    audio_pts: Option<f64>,
    /// Tags dropped because they were older than their track's last one.
    late_drops: u64,
}

pub struct RtmpOutput {
    name: String,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Queued media older than this (by timestamp span) means the network is not
/// keeping up: drop video until the next keyframe.
const MAX_QUEUE_MS: u32 = 3000;

/// One AAC-LC packet at 48 kHz, in seconds: how close the first audio packet
/// has to be to the first keyframe to start the stream on it.
const AAC_FRAME_SECS: f64 = 1024.0 / 48_000.0;

/// How long the first keyframe waits for audio next to it before the stream
/// starts on the keyframe alone (a source whose sound simply starts later).
const START_WAIT: Duration = Duration::from_millis(300);

/// Frames held while the first keyframe waits: bounded, so a source that never
/// produces audio cannot grow the queue.
const MAX_PENDING: usize = 120;

/// How far the picture may run ahead of the sound before video is dropped
/// instead of queued. A server writes one timestamp order across both tracks;
/// audio older than the video it already wrote is discarded ("received too
/// late"), which shifts or holes the recording's sound.
const MAX_AV_SKEW_MS: u32 = 120;

impl RtmpOutput {
    pub fn start(url: &str, key: &str) -> Result<RtmpOutput> {
        let u = parse_url(url)?;
        let name = format!("{}://{}", if u.tls { "rtmps" } else { "rtmp" }, u.host);
        let shared = Arc::new((
            Mutex::new(Shared {
                state: "connecting".into(),
                want_key: true,
                skip_to_key: true,
                // REC_LIVE_HEVC_REFUSE makes the output behave like a server
                // that answered the enhanced-RTMP connect without `hvc1`, which
                // the self-test needs because MediaMTX accepts HEVC.
                hevc_refused: std::env::var_os("REC_LIVE_HEVC_REFUSE").is_some(),
                ..Default::default()
            }),
            Condvar::new(),
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let (sh, st, key) = (shared.clone(), stop.clone(), key.to_string());
        let thread = std::thread::Builder::new()
            .name("rtmp".into())
            .spawn(move || run(u, key, sh, st))?;
        Ok(RtmpOutput {
            name,
            shared,
            stop,
            thread: Some(thread),
        })
    }
}

/// Split a batch into what can go out now and video that must wait for the
/// sound. `ceiling` is the newest audio timestamp plus the allowed skew, or
/// None when the audio is not moving. Audio always goes out; only *video* is
/// held, and held in time order, so it goes out later rather than being lost.
fn hold_for_audio(tags: Vec<Tag>, ceiling: Option<u32>) -> (Vec<Tag>, Vec<Tag>) {
    match ceiling {
        None => (tags, vec![]),
        Some(c) => tags.into_iter().partition(|t| t.typ == 8 || t.ts <= c),
    }
}

fn run(u: RtmpUrl, key: String, shared: Arc<(Mutex<Shared>, Condvar)>, stop: Arc<AtomicBool>) {
    let (lock, cv) = &*shared;
    let mut backoff = 1u64;
    let redact = |e: &anyhow::Error| super::redact(&format!("{e:#}"), &key);
    while !stop.load(Ordering::SeqCst) {
        lock.lock().unwrap().state = "connecting".into();
        let mut conn = match Conn::open(&u, &key) {
            Ok(c) => c,
            Err(e) => {
                let mut s = lock.lock().unwrap();
                s.error = Some(redact(&e));
                s.state = format!("retrying in {backoff}s");
                drop(s);
                eprintln!("rec: stream {}: {}", u.host, redact(&e));
                sleep_unless_stopped(&stop, Duration::from_secs(backoff));
                backoff = (backoff * 2).min(30);
                lock.lock().unwrap().reconnects += 1;
                continue;
            }
        };
        let opened = Instant::now();
        {
            let mut s = lock.lock().unwrap();
            if !conn.hevc_ok && !s.hevc_refused {
                eprintln!(
                    "rec: stream {}: the server does not take HEVC; switching to H.264",
                    u.host
                );
                s.hevc_refused = true;
            }
            s.state = "live".into();
            s.error = None;
            s.live_since.get_or_insert_with(Instant::now);
            // Start the new connection from a keyframe, with a fresh timeline.
            s.queue.retain(|t| t.typ == 8);
            s.skip_to_key = true;
            s.want_key = true;
            s.last_ts = [None; 2];
            s.newest_audio = None;
            s.newest_audio_at = None;
            s.t0 = None;
            s.first_audio_pts = None;
            s.pending_key = None;
            s.pending_key_at = None;
            s.pending_after.clear();
            s.pending_audio.clear();
        }
        let mut headers_sent = false;
        let result: Result<()> = (|| {
            loop {
                if stop.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let (mut tags, formats, wh) = {
                    let mut s = lock.lock().unwrap();
                    if s.queue.is_empty() {
                        s = cv.wait_timeout(s, Duration::from_millis(50)).unwrap().0;
                    }
                    (
                        s.queue.drain(..).collect::<Vec<_>>(),
                        s.formats.clone(),
                        s.width_height,
                    )
                };
                // The audio and video encoders are separate pipelines: their
                // bursts interleave, so a batch can hold tags out of order.
                // Send them by timestamp (stable, so equal timestamps keep
                // their order) — a server discards a sample older than its
                // track (MediaMTX: "received too late, discarding").
                tags.sort_by_key(|t| t.ts);
                // The sound lags the picture at start (and under load) and a
                // server writes one timestamp order across both tracks: the
                // picture waits for the sound rather than being dropped. A
                // dropped H.264 frame corrupts the rest of its GOP, so nothing
                // short of a whole GOP is ever shed (the congestion rule in
                // `packet`).
                let ceiling = {
                    let audio = lock.lock().unwrap();
                    match (audio.newest_audio, audio.newest_audio_at) {
                        (Some(a), Some(at)) if at.elapsed() < Duration::from_millis(500) => {
                            Some(a + MAX_AV_SKEW_MS)
                        }
                        _ => None,
                    }
                };
                let (tags, held) = hold_for_audio(tags, ceiling);
                if !held.is_empty() {
                    let mut s = lock.lock().unwrap();
                    // Put them back in time order ahead of anything newer.
                    let mut held = held;
                    held.extend(s.queue.drain(..));
                    s.queue = held.into();
                }
                if !headers_sent && let Some((hevc, record)) = &formats {
                    let meta = command(&[
                        Amf::Str("@setDataFrame".into()),
                        Amf::Str("onMetaData".into()),
                        Amf::Ecma(vec![
                            ("width".into(), Amf::Number(wh.0 as f64)),
                            ("height".into(), Amf::Number(wh.1 as f64)),
                            (
                                "videocodecid".into(),
                                Amf::Number(if *hevc {
                                    u32::from_be_bytes(*HVC1) as f64
                                } else {
                                    7.0
                                }),
                            ),
                            ("audiocodecid".into(), Amf::Number(10.0)),
                            ("audiosamplerate".into(), Amf::Number(48000.0)),
                            ("stereo".into(), Amf::Bool(true)),
                            ("encoder".into(), Amf::Str("rec-rs".into())),
                        ]),
                    ]);
                    let sid = conn.stream_id;
                    conn.send(C_AUDIO, 18, sid, 0, &meta)?;
                    conn.send(C_VIDEO, 9, sid, 0, &flv_video_header(*hevc, record))?;
                    conn.send(C_AUDIO, 8, sid, 0, &flv_audio_header())?;
                    headers_sent = true;
                }
                if headers_sent {
                    let sid = conn.stream_id;
                    for t in tags {
                        // Per track (audio 8, video 9) time never goes back.
                        let slot = usize::from(t.typ == 9);
                        let mut s = lock.lock().unwrap();
                        let late = s.last_ts[slot].is_some_and(|last| t.ts < last);
                        if late {
                            if std::env::var_os("REC_DEBUG").is_some() {
                                eprintln!(
                                    "rec: debug: rtmp: late {} tag at {} ms (last {})",
                                    if t.typ == 9 { "video" } else { "audio" },
                                    t.ts,
                                    s.last_ts[slot].unwrap_or(0)
                                );
                            }
                            s.late_drops += 1;
                            // A late *video* frame means the GOP it belongs to
                            // is out of order: shed the rest of that GOP too,
                            // never just the one frame (the frames after it
                            // would not decode).
                            if t.typ == 9 {
                                s.queue.retain(|q| q.typ != 9);
                                s.skip_to_key = true;
                            }
                        } else {
                            s.last_ts[slot] = Some(t.ts);
                        }
                        drop(s);
                        if late {
                            if std::env::var_os("REC_DEBUG").is_some() {
                                eprintln!(
                                    "rec: debug: rtmp: {} tag at {} ms is older than the track, dropped",
                                    if t.typ == 9 { "video" } else { "audio" },
                                    t.ts
                                );
                            }
                            continue;
                        }
                        let csid = if t.typ == 9 { C_VIDEO } else { C_AUDIO };
                        conn.send(csid, t.typ, sid, t.ts, &t.body)?;
                    }
                }
                conn.poll()?;
                let mut s = lock.lock().unwrap();
                let now = Instant::now();
                s.samples.push_back((now, conn.sent));
                while s
                    .samples
                    .front()
                    .is_some_and(|(t, _)| now - *t > Duration::from_secs(3))
                {
                    s.samples.pop_front();
                }
                backoff = 1;
            }
        })();
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if let Err(e) = result {
            let mut s = lock.lock().unwrap();
            // HEVC dropped within seconds, twice: assume the server cannot take
            // it.
            if s.formats.as_ref().is_some_and(|f| f.0) && opened.elapsed() < Duration::from_secs(10)
            {
                s.hevc_failures += 1;
                if s.hevc_failures >= 2 && !s.hevc_refused {
                    eprintln!(
                        "rec: stream {}: HEVC keeps failing; switching to H.264",
                        u.host
                    );
                    s.hevc_refused = true;
                }
            }
            s.error = Some(redact(&e));
            s.state = "reconnecting".into();
            s.reconnects += 1;
            s.samples.clear();
            drop(s);
            eprintln!("rec: stream {}: {} (reconnecting)", u.host, redact(&e));
            sleep_unless_stopped(&stop, Duration::from_secs(backoff));
            backoff = (backoff * 2).min(30);
        }
    }
    lock.lock().unwrap().state = "stopped".into();
}

pub(super) fn sleep_unless_stopped(stop: &AtomicBool, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Start the RTMP timeline once a keyframe and an audio packet within one AAC
/// frame of it are both known: the two tracks then begin together and no audio
/// is older than the stream's start. A stream without audio starts 300 ms after
/// its first keyframe anyway.
fn maybe_start(shared: &Arc<(Mutex<Shared>, Condvar)>, latest_audio: Option<f64>) {
    let (lock, cv) = &**shared;
    let mut s = lock.lock().unwrap();
    let Some((key_pts, _)) = s.pending_key.clone() else {
        return;
    };
    let audio = latest_audio.or(s.first_audio_pts);
    let aligned = audio.is_some_and(|a| (a - key_pts).abs() <= AAC_FRAME_SECS + 0.01);
    let timed_out = s.pending_key_at.is_some_and(|t| t.elapsed() > START_WAIT);
    if !aligned && !timed_out {
        return;
    }
    // Aligned: both tracks start together. Timed out: the sound starts later
    // than the picture, so the timeline starts on the keyframe and the audio
    // joins it at its own time.
    let t0 = if aligned {
        audio.map_or(key_pts, |a| a.min(key_pts))
    } else {
        key_pts
    };
    let (key_pts, mut key) = s.pending_key.take().unwrap();
    key.ts = ((key_pts - t0) * 1000.0).round().max(0.0) as u32;
    s.pending_key_at = None;
    s.started = Some(Instant::now());
    s.audio_pts = audio;
    s.t0 = Some(t0);
    // The keyframe first, then everything that waited, stamped from the
    // timeline.
    let after = std::mem::take(&mut s.pending_after);
    let queued_audio = std::mem::take(&mut s.pending_audio);
    s.queue.push_back(key);
    // The keyframe that just started the stream is the keyframe the "drop
    // until the next keyframe" state was waiting for: everything after it goes
    // out (dropping it would corrupt the GOP).
    s.skip_to_key = false;
    for (pts, mut tag) in after.into_iter().chain(queued_audio) {
        if pts < t0 - 1e-9 {
            continue; // older than the stream
        }
        tag.ts = ((pts - t0) * 1000.0).round().max(0.0) as u32;
        s.queue.push_back(tag);
    }
    if std::env::var_os("REC_DEBUG").is_some() {
        eprintln!(
            "rec: debug: rtmp: stream starts with the keyframe {:+.1} ms from the audio",
            (key_pts - t0) * 1000.0
        );
    }
    cv.notify_one();
}

impl Output for RtmpOutput {
    fn packet(&mut self, p: &EncodedPacket, f: &Formats) {
        let (lock, cv) = &*self.shared;
        maybe_start(&self.shared, None);
        let mut s = lock.lock().unwrap();
        match p.kind {
            PacketKind::Video(_) => {
                let hevc = f.hevc;
                // New codec configuration (first keyframe, or the encoder
                // switched codec or size): a sequence header goes ahead of it.
                if p.keyframe
                    && !f.record.is_empty()
                    && s.formats.as_ref() != Some(&(hevc, f.record.clone()))
                {
                    let had = s.formats.is_some();
                    s.formats = Some((hevc, f.record.clone()));
                    if had && let Some(t0) = s.t0 {
                        let ts = ((p.pts - t0) * 1000.0).round().max(0.0) as u32;
                        s.queue.push_back(Tag {
                            typ: 9,
                            ts,
                            body: Arc::new(flv_video_header(hevc, &f.record)),
                        });
                    }
                }
                if s.formats.as_ref().is_none_or(|(h, _)| *h != hevc) {
                    return;
                }
                if s.t0.is_none() {
                    if p.keyframe && s.pending_key.is_none() {
                        // Held until an audio packet next to it arrives (see
                        // `pending_key`); `maybe_start` takes it from here.
                        s.pending_key = Some((
                            p.pts,
                            Tag {
                                typ: 9,
                                ts: 0,
                                body: Arc::new(flv_video(hevc, true, &p.data)),
                            },
                        ));
                        s.pending_key_at = Some(Instant::now());
                    } else if s.pending_key.is_some() && s.pending_after.len() < MAX_PENDING {
                        // A frame after the held keyframe belongs to the
                        // stream: it waits with the keyframe instead of being
                        // dropped (a dropped frame corrupts its whole GOP).
                        s.pending_after.push((
                            p.pts,
                            Tag {
                                typ: 9,
                                ts: 0,
                                body: Arc::new(flv_video(hevc, p.keyframe, &p.data)),
                            },
                        ));
                    }
                    cv.notify_one();
                    return;
                }
                if s.width_height == (0, 0) {
                    s.width_height = (f.width, f.height);
                }
                if s.skip_to_key {
                    if !p.keyframe {
                        s.dropped += 1;
                        return;
                    }
                    s.skip_to_key = false;
                }
                let ts = ((p.pts - s.t0.unwrap()) * 1000.0).round().max(0.0) as u32;
                // Congestion: the queue spans too much time.
                if let (Some(first), true) = (s.queue.front().map(|t| t.ts), !p.keyframe)
                    && ts.saturating_sub(first) > MAX_QUEUE_MS
                {
                    if std::env::var_os("REC_DEBUG").is_some() {
                        eprintln!(
                            "rec: debug: rtmp: congestion at {ts} ms (queue front {first}): shedding the GOP"
                        );
                    }
                    s.queue.retain(|t| t.typ != 9);
                    s.skip_to_key = true;
                    s.dropped += 1;
                    return;
                }
                s.queue.push_back(Tag {
                    typ: 9,
                    ts,
                    body: Arc::new(flv_video(hevc, p.keyframe, &p.data)),
                });
            }
            PacketKind::Audio(AudioCodec::Aac) => {
                if s.t0.is_none() {
                    // Before the timeline exists, audio feeds the start
                    // decision and waits with the keyframe.
                    s.first_audio_pts = s.first_audio_pts.or(Some(p.pts));
                    if s.pending_key.is_some() && s.pending_audio.len() < MAX_PENDING {
                        s.pending_audio.push((
                            p.pts,
                            Tag {
                                typ: 8,
                                ts: 0,
                                body: Arc::new(flv_audio(&p.data)),
                            },
                        ));
                    }
                    drop(s);
                    maybe_start(&self.shared, Some(p.pts));
                    cv.notify_one();
                    return;
                }
                let t0 = s.t0.unwrap();
                if p.pts < t0 - 1e-9 {
                    return;
                }
                let ts = ((p.pts - t0) * 1000.0).round().max(0.0) as u32;
                if s.queue.len() > 2000 {
                    return;
                }
                s.newest_audio = Some(s.newest_audio.map_or(ts, |a| a.max(ts)));
                s.newest_audio_at = Some(Instant::now());
                s.audio_pts = Some(p.pts);
                s.queue.push_back(Tag {
                    typ: 8,
                    ts,
                    body: Arc::new(flv_audio(&p.data)),
                });
            }
            PacketKind::Audio(AudioCodec::Opus) => return,
        }
        cv.notify_one();
    }

    fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.shared.0.lock().unwrap().want_key)
    }

    fn accepts_hevc(&self) -> bool {
        !self.shared.0.lock().unwrap().hevc_refused
    }

    fn status(&self) -> OutputStatus {
        let s = self.shared.0.lock().unwrap();
        let bitrate = match (s.samples.front(), s.samples.back()) {
            (Some(a), Some(b)) if s.state == "live" && b.0 > a.0 => {
                b.1.saturating_sub(a.1) as f64 * 8.0 / (b.0 - a.0).as_secs_f64()
            }
            _ => 0.0,
        };
        OutputStatus {
            name: self.name.clone(),
            state: s.state.clone(),
            live: s.state == "live",
            bitrate,
            dropped_frames: s.dropped,
            reconnects: s.reconnects,
            duration: s.live_since.map_or(0.0, |t| t.elapsed().as_secs_f64()),
            error: s.error.clone(),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.shared.1.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for RtmpOutput {
    fn drop(&mut self) {
        Output::stop(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amf_round_trip() {
        let v = vec![
            Amf::Str("connect".into()),
            Amf::Number(1.0),
            obj(&[("app", Amf::Str("live".into())), ("fpad", Amf::Bool(false))]),
            Amf::Null,
            Amf::Ecma(vec![("width".into(), Amf::Number(1920.0))]),
        ];
        let bytes = command(&v);
        assert_eq!(amf_all(&bytes), v);
        assert_eq!(&bytes[..3], &[2, 0, 7]);
    }

    #[test]
    fn urls() {
        let u = parse_url("rtmps://a.rtmps.youtube.com:443/live2").unwrap();
        assert_eq!(
            (u.tls, u.host.as_str(), u.port, u.app.as_str()),
            (true, "a.rtmps.youtube.com", 443, "live2")
        );
        assert_eq!(u.tc_url, "rtmps://a.rtmps.youtube.com:443/live2");
        let u = parse_url("rtmp://localhost/live/").unwrap();
        assert_eq!(
            (u.port, u.app.as_str(), u.tc_url.as_str()),
            (1935, "live", "rtmp://localhost/live")
        );
        assert!(parse_url("http://x/y").is_err());
    }

    #[test]
    fn chunking_round_trips_through_the_reader() {
        let payload: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let mut wire = vec![];
        // The reader starts at 128-byte chunks; announce 4096 first.
        wire.extend(chunk_message(2, 1, 0, 0, &4096u32.to_be_bytes(), 4096));
        wire.extend(chunk_message(6, 9, 1, 1234, &payload, 4096));
        wire.extend(chunk_message(6, 9, 1, 0x0100_0000, b"ext", 4096));
        let mut r = ChunkReader::default();
        // Feed in odd pieces like a socket would.
        let mut msgs = vec![];
        for part in wire.chunks(777) {
            msgs.extend(r.feed(part));
        }
        assert_eq!(msgs.len(), 3);
        assert_eq!(
            msgs[1],
            Message {
                typ: 9,
                sid: 1,
                payload
            }
        );
        assert_eq!(msgs[2].payload, b"ext");
    }

    /// Enhanced RTMP is negotiable per server: a codec list without `hvc1`
    /// refuses HEVC, one that lists nothing may take it.
    #[test]
    fn hevc_negotiation() {
        let with_hvc1 = obj(&[(
            "fourCcList",
            Amf::Array(vec![Amf::Str("hvc1".into()), Amf::Str("avc1".into())]),
        )]);
        assert!(hevc_allowed(&[
            Amf::Str("_result".into()),
            Amf::Number(1.0),
            with_hvc1
        ]));
        let wildcard = obj(&[("fourCcList", Amf::Array(vec![Amf::Str("*".into())]))]);
        assert!(hevc_allowed(&[wildcard]));
        let h264_only = obj(&[("fourCcList", Amf::Array(vec![Amf::Str("avc1".into())]))]);
        assert!(!hevc_allowed(&[h264_only]));
        let quiet = obj(&[("fpad", Amf::Bool(false))]);
        assert!(hevc_allowed(&[Amf::Str("_result".into()), quiet]));
    }

    /// An output with no connection: `packet` only queues, so the start burst
    /// can be fed to it directly.
    fn offline() -> RtmpOutput {
        let shared = Arc::new((
            Mutex::new(Shared {
                state: "offline".into(),
                ..Default::default()
            }),
            Condvar::new(),
        ));
        RtmpOutput {
            name: "test".into(),
            shared,
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    fn video_packet(pts: f64, key: bool) -> EncodedPacket {
        EncodedPacket {
            pts,
            dts: pts,
            keyframe: key,
            data: Arc::from(vec![0u8; 64]),
            kind: PacketKind::Video(crate::media_types::VideoCodec::H264),
        }
    }

    fn audio_packet(pts: f64) -> EncodedPacket {
        EncodedPacket {
            pts,
            dts: pts,
            keyframe: true,
            data: Arc::from(vec![0u8; 32]),
            kind: PacketKind::Audio(AudioCodec::Aac),
        }
    }

    fn formats() -> Formats {
        Formats {
            hevc: false,
            record: vec![1, 0x64, 0, 0x28, 0xff, 0xe1, 0, 0, 0],
            param_sets: vec![vec![0x67, 0x64, 0, 0x28]],
            aac_cookie: vec![0x11, 0x90],
            width: 1280,
            height: 720,
            ..Default::default()
        }
    }

    /// The start burst with four destinations on one encode: the picture runs
    /// ~150 ms ahead of the sound while the audio encoder warms up. Nothing may
    /// be dropped — an encoded H.264 frame is only useful with the frames after
    /// it, so a mid-GOP drop corrupts the picture up to the next keyframe. The
    /// start waits for audio, and frames that run ahead are *held*, then sent.
    #[test]
    fn start_burst_never_drops_frames() {
        let mut out = offline();
        let f = formats();
        // One keyframe at 10.000, then 60 frames (2 s at 30 fps); the audio
        // starts 150 ms later and trails the picture by 150 ms throughout.
        out.packet(&video_packet(10.0, true), &f);
        let mut audio_done = 0;
        // Frames 1..60 after the keyframe (frame 30 is the next keyframe).
        for i in 1..=60 {
            let t = 10.0 + i as f64 / 30.0;
            out.packet(&video_packet(t, i == 30), &f);
            // Audio is queued in bursts, always behind the video.
            while audio_done as f64 * 0.021_333 + 10.15 <= t + 0.15 {
                out.packet(&audio_packet(10.15 + audio_done as f64 * 0.021_333), &f);
                audio_done += 1;
            }
        }
        {
            let s = out.shared.0.lock().unwrap();
            assert_eq!(s.dropped, 0, "no frame may be dropped at start");
            assert!(!s.skip_to_key, "the stream keeps flowing");
            // The keyframe waits and every frame after it waits with it.
            assert!(s.pending_key.is_some(), "the start waits for audio");
            assert_eq!(
                s.pending_after.len(),
                60,
                "all 60 frames after the keyframe are held, not dropped"
            );
        }
        // A source whose sound never comes close to the keyframe still starts,
        // on the keyframe, and every held frame goes out.
        std::thread::sleep(START_WAIT + Duration::from_millis(20));
        out.packet(&audio_packet(10.3), &f);
        let s = out.shared.0.lock().unwrap();
        let queued_video = s.queue.iter().filter(|t| t.typ == 9).count();
        assert_eq!(queued_video, 61, "61 frames queued after the start");
        assert_eq!(s.dropped, 0, "still nothing dropped");
        // The keyframe starts the timeline at 0 and the frames follow at the
        // frame grid.
        let key = s.queue.iter().find(|t| t.typ == 9).unwrap().ts;
        assert_eq!(key, 0, "the keyframe starts at 0");
        let spans: Vec<u32> = s
            .queue
            .iter()
            .filter(|t| t.typ == 9)
            .map(|t| t.ts)
            .collect();
        assert!(
            spans.windows(2).all(|w| w[1] > w[0]),
            "frames stay in order: {spans:?}"
        );
        assert_eq!(spans.last(), Some(&2000), "2 s of frames");
        assert!(
            !s.skip_to_key,
            "the start clears the drop-until-keyframe state"
        );
        drop(s);
        // Frames after the start go out too (the regression this test is for:
        // `skip_to_key` stayed set after the held keyframe started the stream,
        // so the next ~1 s of frames was dropped mid-GOP).
        for i in 61..=64 {
            out.packet(&video_packet(10.0 + i as f64 / 30.0, false), &f);
        }
        let s = out.shared.0.lock().unwrap();
        assert_eq!(s.dropped, 0, "still nothing dropped");
        assert_eq!(
            s.queue.iter().filter(|t| t.typ == 9).count(),
            65,
            "every frame after the start is queued"
        );
    }

    /// Holding is what keeps the tracks in step: video beyond the audio's reach
    /// goes back into the queue in time order, audio always goes out.
    #[test]
    fn the_picture_waits_for_the_sound() {
        let tag = |typ: u8, ts: u32| Tag {
            typ,
            ts,
            body: Arc::new(vec![]),
        };
        let tags = vec![
            tag(8, 100),
            tag(9, 120),
            tag(8, 130),
            tag(9, 150),
            tag(9, 200),
        ];
        let (send, held) = hold_for_audio(tags, Some(130));
        assert_eq!(
            send.iter().map(|t| (t.typ, t.ts)).collect::<Vec<_>>(),
            vec![(8, 100), (9, 120), (8, 130)]
        );
        assert_eq!(
            held.iter().map(|t| (t.typ, t.ts)).collect::<Vec<_>>(),
            vec![(9, 150), (9, 200)]
        );
        // With the audio stalled, nothing is held back forever.
        let (send, held) = hold_for_audio(vec![tag(9, 900)], None);
        assert_eq!((send.len(), held.len()), (1, 0));
    }

    #[test]
    fn flv_bodies() {
        assert_eq!(flv_audio_header(), vec![0xAF, 0, 0x11, 0x90]);
        assert_eq!(&flv_video(false, true, &[1, 2])[..5], &[0x17, 1, 0, 0, 0]);
        assert_eq!(flv_video(false, false, &[9])[0], 0x27);
        let h = flv_video_header(false, &[1, 0x64, 0, 0x28]);
        assert_eq!(&h[..6], &[0x17, 0, 0, 0, 0, 1]);
        // Enhanced RTMP: IsExHeader | frame type | packet type, then FourCC.
        assert_eq!(&flv_video_header(true, &[1])[..5], b"\x90hvc1");
        assert_eq!(&flv_video(true, true, &[7])[..6], b"\x93hvc1\x07");
        assert_eq!(flv_video(true, false, &[7])[0], 0xA3);
        let mut b = vec![];
        Amf::Array(vec![Amf::Str("hvc1".into()), Amf::Number(1.0)]).encode(&mut b);
        assert_eq!(
            Amf::decode(&b).unwrap().0,
            Amf::Array(vec![Amf::Str("hvc1".into()), Amf::Number(1.0)])
        );
    }
}
