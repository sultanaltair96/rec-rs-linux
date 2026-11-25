//! Local diagnostics with bounded, redacted stderr capture and typed events.
//! Reports are never uploaded automatically. Unknown native lines are dropped;
//! reviewed application lines still require review before sharing.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use serde::Serialize;

const CAPACITY: usize = 5000;
const MAX_LINE: usize = 4096;
// Bytes discarded by the bounded forwarding queue or rejected by the original
// destination (including an originally nonblocking fd returning EAGAIN).
static DROPPED_STDERR_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DROPPED_LINES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn logs() -> &'static Mutex<VecDeque<String>> {
    static LOGS: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    LOGS.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAPACITY)))
}

// Fail closed for credential-bearing lines, including JSON, CLI flags, URLs,
// authorization headers. Never retain their raw content.
fn redact(line: &[u8], key: Option<&str>) -> String {
    let text = String::from_utf8_lossy(line);
    let lower = text.to_ascii_lowercase();
    if [
        "key",
        "token",
        "secret",
        "password",
        "authorization",
        "bearer",
        "://",
    ]
    .iter()
    .any(|s| lower.contains(s))
        || key
            .filter(|s| !s.is_empty())
            .is_some_and(|s| text.contains(s))
    {
        "[redacted credential/URL line]".into()
    } else {
        text.into_owned()
    }
}

#[derive(Default)]
struct Lines {
    pending: Vec<u8>,
    oversized: bool,
}

impl Lines {
    fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(&[u8])) {
        for &byte in bytes {
            if byte == b'\n' {
                if self.oversized {
                    DROPPED_LINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                } else {
                    emit(&self.pending);
                }
                self.pending.clear();
                self.oversized = false;
            } else if !self.oversized {
                if self.pending.len() == MAX_LINE {
                    self.pending.clear();
                    self.oversized = true;
                } else {
                    self.pending.push(byte);
                }
            }
        }
    }

    fn finish(&mut self, mut emit: impl FnMut(&[u8])) {
        if self.oversized {
            DROPPED_LINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else if !self.pending.is_empty() {
            emit(&self.pending);
        }
        self.pending.clear();
        self.oversized = false;
    }
}

fn secrets() -> &'static Mutex<Vec<String>> {
    static SECRETS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    SECRETS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register before passing credentials to libraries that may log them. Keep old
/// values for the lifetime of this process (including after rotation/deletion).
/// Scrub retained lines too; the retention worker rechecks queued lines.
pub fn register_secret(secret: &str) {
    if secret.is_empty() {
        return;
    }
    let mut secrets = secrets().lock().unwrap_or_else(|e| e.into_inner());
    for part in std::iter::once(secret).chain(secret.split(['\r', '\n'])) {
        if part.is_empty() {
            continue;
        }
        for variant in secret_variants(part) {
            if !secrets.contains(&variant) {
                secrets.push(variant);
            }
        }
    }
    let mut ring = logs().lock().unwrap_or_else(|e| e.into_inner());
    ring.retain(|line| {
        let keep = !secrets.iter().any(|s| line.contains(s));
        if !keep {
            DROPPED_LINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        keep
    });
}

fn secret_variants(secret: &str) -> Vec<String> {
    let mut variants = vec![secret.to_owned()];
    for upper in [false, true] {
        // Include both conventional URL escaping and fully escaped bytes.
        for all in [false, true] {
            let mut encoded = String::new();
            for b in secret.bytes() {
                if !all && (b.is_ascii_alphanumeric() || b"-._~".contains(&b)) {
                    encoded.push(b as char);
                } else if upper {
                    encoded.push_str(&format!("%{b:02X}"));
                } else {
                    encoded.push_str(&format!("%{b:02x}"));
                }
            }
            variants.push(encoded.replace("%20", "+"));
            variants.push(encoded);
        }
    }
    for alphabet in [
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_",
    ] {
        let mut encoded = String::new();
        for chunk in secret.as_bytes().chunks(3) {
            let n = ((chunk[0] as u32) << 16)
                | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
                | chunk.get(2).copied().unwrap_or(0) as u32;
            for shift in [18, 12, 6, 0].into_iter().take(chunk.len() + 1) {
                encoded.push(alphabet[((n >> shift) & 63) as usize] as char);
            }
        }
        variants.push(encoded.clone());
        while encoded.len() % 4 != 0 {
            encoded.push('=');
        }
        variants.push(encoded);
    }
    variants
}

// Unknown native output is never retained. Native exceptions must be exact,
// reviewed literals, not broad GST/PW prefixes or substring matches.
const NATIVE_WARNINGS: &[&str] = &[
    "GStreamer warning: failed to set pipeline to PAUSED",
    "PipeWire warning: stream disconnected",
];

fn filter_line(line: &[u8], secrets: &[String]) -> Option<String> {
    let text = std::str::from_utf8(line).ok()?;
    if line.len() > MAX_LINE
        || text.chars().any(char::is_control)
        || secrets.iter().any(|s| text.contains(s))
    {
        return None;
    }
    if !text.starts_with("rec: ") && !NATIVE_WARNINGS.contains(&text) {
        return None;
    }
    if NATIVE_WARNINGS.contains(&text) {
        return Some(text.to_owned());
    }
    let lower = text.to_ascii_lowercase();
    if ["rtmp", "whip", "stream", "transport"]
        .iter()
        .any(|s| lower.contains(s))
    {
        return Some("rec: live transport diagnostic (details omitted)".into());
    }
    let safe = redact(line, None);
    if safe != text { None } else { Some(safe) }
}

fn retain_line(line: String) {
    // Same lock order as registration: queued lines cannot race a rotation.
    let secrets = secrets().lock().unwrap_or_else(|e| e.into_inner());
    let Some(line) = filter_line(line.as_bytes(), &secrets) else {
        DROPPED_LINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    };
    // Diagnostics must never wait for an exporting/reporting thread.
    match logs().try_lock() {
        Ok(mut ring) => {
            if ring.len() == CAPACITY {
                ring.pop_front();
            }
            ring.push_back(line);
        }
        Err(_) => {
            DROPPED_LINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

struct TeeShutdown {
    original: std::fs::File,
    done: std::sync::mpsc::Receiver<()>,
}
static TEE_SHUTDOWN: OnceLock<Mutex<Option<TeeShutdown>>> = OnceLock::new();

extern "C" fn shutdown_tee() {
    use std::os::fd::AsRawFd;
    if let Some(state) = TEE_SHUTDOWN.get()
        && let Ok(mut state) = state.lock()
        && let Some(state) = state.take()
    {
        // Restore first, then allow already-written bytes to reach the original
        // destination. Bound the wait: a child may still hold inherited fd 2.
        unsafe {
            libc::dup2(state.original.as_raw_fd(), 2);
        }
        let _ = state
            .done
            .recv_timeout(std::time::Duration::from_millis(250));
    }
}

/// Process-wide fd-level tee, including native-library stderr. The original fd
/// remains the best-effort output destination through a bounded forwarding queue.
/// Slow output drops bytes (counted), rather than stalling capture. fd 2 is
/// blocking: the running reader drains independently of the output worker.
/// A starved/paused reader can still block writers on this finite pipe: bounded
/// queues isolate slow consumers, not scheduler starvation of the drain itself.
/// Install only once, before starting library threads. No raw log is persisted.
/// Environment keys are refreshed per line; app key use points register stored
/// credentials before handing them to streaming libraries.
pub fn install_stderr_tee() -> std::io::Result<()> {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    static INSTALLED: Mutex<bool> = Mutex::new(false);
    let mut installed = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
    if *installed {
        return Ok(());
    }
    if let Ok(key) = std::env::var("REC_STREAM_KEY") {
        register_secret(&key);
    }
    // Owned descriptors are CLOEXEC, so subprocesses cannot keep the pipe alive.
    let original = unsafe { libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 3) };
    if original < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut original = unsafe { std::fs::File::from_raw_fd(original) };
    let restore = original.try_clone()?;
    let mut pipe = [0; 2];
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut input = unsafe { std::fs::File::from_raw_fd(pipe[0]) };
    let output = unsafe { std::fs::File::from_raw_fd(pipe[1]) };
    let (tx, rx) = std::sync::mpsc::sync_channel::<String>(256);
    std::thread::Builder::new()
        .name("diagnostic-ring".into())
        .spawn(move || {
            for line in rx {
                // Registration scrubs the ring and may wait for an exporter.
                // Keep it off the fd drain; retention rechecks queued content.
                if let Ok(key) = std::env::var("REC_STREAM_KEY") {
                    register_secret(&key);
                }
                retain_line(line);
            }
        })?;
    // Isolate a potentially blocking terminal/pipe/file from the capture reader.
    // Only this worker can wait for the original destination.
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let (output_tx, output_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(32);
    std::thread::Builder::new()
        .name("stderr-output".into())
        .spawn(move || {
            for bytes in output_rx {
                let mut remaining = bytes.as_slice();
                while !remaining.is_empty() {
                    match original.write(remaining) {
                        Ok(0) => break,
                        Ok(n) => remaining = &remaining[n..],
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                DROPPED_STDERR_BYTES
                    .fetch_add(remaining.len() as u64, std::sync::atomic::Ordering::Relaxed);
            }
            let _ = done_tx.send(());
        })?;
    // fd 2 stays blocking: native/Rust writers must not see short writes or
    // EAGAIN. Enlargement is best effort (unprivileged kernels may deny it).
    unsafe {
        libc::fcntl(pipe[1], libc::F_SETPIPE_SZ, 1024 * 1024);
    }
    let panic_restore = restore.try_clone()?;
    // Start before redirecting, so a failed spawn leaves stderr untouched.
    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let reader = std::thread::Builder::new()
        .name("stderr-tee".into())
        .spawn(move || {
            if start_rx.recv().is_err() {
                return;
            }
            // Restore fd 2 even if parsing panics, before closing the reader.
            struct Restore(std::fs::File);
            impl Drop for Restore {
                fn drop(&mut self) {
                    use std::os::fd::AsRawFd;
                    unsafe {
                        libc::dup2(self.0.as_raw_fd(), 2);
                    }
                }
            }
            let _restore = Restore(panic_restore);
            let mut lines = Lines::default();
            let emit = |line: &[u8]| {
                // Registration can hold secrets while waiting to scrub an
                // exported ring. The drain must drop, never wait behind it.
                let Ok(secrets) = secrets().try_lock() else {
                    DROPPED_LINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return;
                };
                let safe = filter_line(line, &secrets);
                drop(secrets);
                let Some(safe) = safe else {
                    DROPPED_LINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return;
                };
                if tx.try_send(safe).is_err() {
                    DROPPED_LINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            };
            let mut bytes = [0; 8192];
            loop {
                match input.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(n) => {
                        if output_tx.try_send(bytes[..n].to_vec()).is_err() {
                            DROPPED_STDERR_BYTES
                                .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                        }
                        lines.feed(&bytes[..n], emit);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            lines.finish(emit);
        })?;
    if unsafe { libc::dup2(pipe[1], 2) } < 0 {
        drop(start_tx);
        let _ = reader.join();
        return Err(std::io::Error::last_os_error());
    }
    drop(output);
    *installed = true;
    *TEE_SHUTDOWN
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(TeeShutdown {
        original: restore,
        done: done_rx,
    });
    unsafe {
        libc::atexit(shutdown_tee);
    }
    let _ = start_tx.send(());
    Ok(())
}

/// Safe vocabulary shared with the app. Extend deliberately; never add free text.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    AppStarted,
    RecordingStarted,
    RecordingStopped,
    CaptureUnavailable,
    EncoderUnavailable,
    PermissionDenied,
    ReportRequested,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Entry {
    pub sequence: u64,
    pub event: Event,
}

/// Fixed-size, process-local ring. No disk persistence and no raw text input.
pub struct Ring {
    entries: VecDeque<Entry>,
    next: u64,
}

impl Default for Ring {
    fn default() -> Self {
        Self {
            entries: VecDeque::with_capacity(CAPACITY),
            next: 0,
        }
    }
}

impl Ring {
    pub fn push(&mut self, event: Event) {
        if self.entries.len() == CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(Entry {
            sequence: self.next,
            event,
        });
        self.next = self.next.saturating_add(1);
    }

    pub fn snapshot(&self) -> Vec<Entry> {
        self.entries.iter().copied().collect()
    }
}

fn ring() -> &'static Mutex<Ring> {
    static RING: OnceLock<Mutex<Ring>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(Ring::default()))
}

/// App integration: call at lifecycle transitions, never with user data.
pub fn record(event: Event) {
    ring().lock().unwrap_or_else(|e| e.into_inner()).push(event);
}

pub fn snapshot() -> Vec<Entry> {
    ring().lock().unwrap_or_else(|e| e.into_inner()).snapshot()
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    app_version: &'static str,
    os: &'static str,
    architecture: &'static str,
    session_type: &'static str,
    sandbox: &'static str,
    gstreamer_version: String,
    encoders: Vec<(&'static str, bool)>,
    events: Vec<Entry>,
    logs: Vec<String>,
    dropped_log_lines: u64,
    dropped_stderr_bytes: u64,
    settings: serde_json::Value,
    devices: serde_json::Value,
    recordings: serde_json::Value,
    platform: serde_json::Value,
    collection_status: serde_json::Value,
    privacy: &'static str,
}

// Copy only typed numeric/boolean values. Never serialize the settings object:
// it contains destinations, device identifiers and private filesystem paths.
fn sanitized_settings() -> serde_json::Value {
    let s = crate::settings::Settings::load();
    serde_json::json!({
        "fps": s.fps, "bitrate_mbps": s.bitrate_mbps,
        "mic_enabled": s.mic_enabled, "system_audio": s.system_audio,
        "system_gain_db": s.system_gain_db, "mic_gain_db": s.mic_gain_db,
        "system_muted": s.system_muted, "mic_muted": s.mic_muted,
        "cursor": s.cursor, "clicks": s.clicks,
        "preview_low_power": s.preview_low_power,
        "replay_enabled": s.replay_enabled, "replay_secs": s.replay_secs,
        "stream_bitrate_kbps": s.stream_bitrate_kbps,
        "stream_audio_kbps": s.stream_audio_kbps,
        "keyframe_secs": s.keyframe_secs,
        "source": "persisted settings (or defaults when unavailable); not live app state"
    })
}

// Only map known values to constants: arbitrary environment strings are private.
fn session_type(value: Option<&str>) -> &'static str {
    match value {
        Some("wayland") => "wayland",
        Some("x11") => "x11",
        Some("tty") => "tty",
        _ => "unknown",
    }
}

// Names are the only free-text exception. Omit the entire name when it contains
// an identifier: partial stripping can leave serial fragments behind. This is
// deliberately conservative, not a guarantee for arbitrary vendor descriptions.
fn identifying_name(lower: &str) -> bool {
    if [
        "serial",
        "s/n",
        "sn:",
        "sn=",
        "by-id",
        "by_id",
        "alsa_input.",
        "alsa_output.",
        "usb-",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return true;
    }
    // Bluetooth/PipeWire descriptions can use colons, hyphens, underscores or
    // dotted MAC addresses. Scan bytes so surrounding punctuation cannot hide it.
    let bytes = lower.as_bytes();
    for (groups, width, separators) in [(6, 2, &b":-_"[..]), (3, 4, &b"."[..])] {
        let len = groups * width + groups - 1;
        if bytes.windows(len).any(|candidate| {
            let separator = candidate[width];
            separators.contains(&separator)
                && candidate.iter().enumerate().all(|(i, b)| {
                    if (i + 1) % (width + 1) == 0 {
                        *b == separator
                    } else {
                        b.is_ascii_hexdigit()
                    }
                })
        }) {
            return true;
        }
    }
    false
}

fn safe_name(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    if name.is_empty()
        || name.len() > 128
        || identifying_name(&lower)
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '=' | '@'))
        || ["token", "secret", "password", "key="]
            .iter()
            .any(|s| lower.contains(s))
    {
        None
    } else {
        Some(name.to_owned())
    }
}

fn names(result: anyhow::Result<Vec<String>>) -> serde_json::Value {
    match result {
        Ok(items) => {
            serde_json::json!({"status": "enumerated", "names": items.iter().take(64).filter_map(|s| safe_name(s)).collect::<Vec<_>>()})
        }
        Err(_) => serde_json::json!({"status": "unavailable"}),
    }
}

fn device_names() -> serde_json::Value {
    serde_json::json!({
        "cameras": names(crate::capture::camera::cameras().map(|v| v.into_iter().map(|d| d.name).collect())),
        "audio_inputs": names(crate::capture::audio::microphones().map(|v| v.into_iter().map(|d| d.name).collect())),
        "monitors": names(crate::capture::screen::displays().map(|v| v.into_iter().map(|d| d.name).collect()))
    })
}

fn platform_info() -> serde_json::Value {
    serde_json::json!({
        "drm_present": Path::new("/dev/dri").is_dir(),
        "portal": crate::capture::screen::portal_availability().ok().map(|p| serde_json::json!({
            "version": p.version, "monitors": p.monitors, "windows": p.windows, "cursor_modes": p.cursor_modes
        })),
        "va_encoders": (["vah264enc", "vah265enc", "vaapih264enc", "vaapih265enc"].map(|n| (n, gstreamer::ElementFactory::find(n).is_some())))
    })
}

// ffprobe output is untrusted: rebuild a summary, never copy objects or strings.
fn probe_summary(value: &serde_json::Value) -> serde_json::Value {
    let streams = value["streams"]
        .as_array()
        .into_iter()
        .flatten()
        .take(16)
        .map(|s| {
            let mut out = serde_json::Map::new();
            for key in [
                "width",
                "height",
                "channels",
                "sample_rate",
                "duration",
                "bit_rate",
            ] {
                let n = s[key]
                    .as_f64()
                    .or_else(|| s[key].as_str()?.parse::<f64>().ok());
                if let Some(n) = n.filter(|n| n.is_finite() && *n >= 0.0 && *n <= 1e15) {
                    out.insert(key.into(), serde_json::json!(n));
                }
            }
            for (key, allowed) in [
                ("codec_type", &["video", "audio"][..]),
                (
                    "codec_name",
                    &[
                        "h264",
                        "hevc",
                        "prores",
                        "aac",
                        "opus",
                        "pcm_s16le",
                        "vp9",
                        "av1",
                    ][..],
                ),
            ] {
                if let Some(s) = s[key].as_str().filter(|s| allowed.contains(s)) {
                    out.insert(key.into(), serde_json::json!(s));
                }
            }
            serde_json::Value::Object(out)
        })
        .collect::<Vec<_>>();
    serde_json::json!({"streams": streams})
}

fn probe_file(path: &Path) -> Option<serde_json::Value> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            "mov,matroska,webm",
            "-show_entries",
            "stream=codec_type,codec_name,width,height,channels,sample_rate,duration,bit_rate",
            "-of",
            "json",
            "-i",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.take(65537).read_to_end(&mut bytes);
        bytes
    });
    let start = std::time::Instant::now();
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if start.elapsed() < std::time::Duration::from_secs(5) => {
                std::thread::sleep(std::time::Duration::from_millis(25))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let bytes = reader.join().ok()?;
    if !success || bytes.len() > 65536 {
        return None;
    }
    Some(probe_summary(
        &serde_json::from_slice::<serde_json::Value>(&bytes).ok()?,
    ))
}

fn recent_recordings() -> serde_json::Value {
    let dir = crate::settings::Settings::load().output_dir;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return serde_json::json!({"status": "directory_unavailable", "limit": 5});
    };
    let mut files = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            if !entry.file_type().ok()?.is_file() {
                return None;
            }
            let path = entry.path();
            if !matches!(path.extension()?.to_str()?, "mp4" | "mov" | "mkv") {
                return None;
            }
            Some((entry.metadata().ok()?.modified().ok()?, path))
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|a| std::cmp::Reverse(a.0));
    serde_json::json!({"limit": 5, "summaries": files.iter().take(5).map(|(_, p)| probe_file(p).unwrap_or_else(|| serde_json::json!({"status": "probe_unavailable_or_failed"}))).collect::<Vec<_>>()})
}

static GUI_PROCESS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Explicit GUI provenance; AppStarted is also emitted by standalone commands.
pub fn mark_gui_process() {
    GUI_PROCESS.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn history_status(gui: bool) -> &'static str {
    if gui {
        "gui_process: includes this app process only; no previous-process or crash history"
    } else {
        "standalone_process: does not collect a running GUI's logs or previous-process/crash history"
    }
}

fn report() -> Report {
    Report {
        schema_version: 1,
        app_version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        session_type: session_type(std::env::var("XDG_SESSION_TYPE").ok().as_deref()),
        sandbox: if Path::new("/.flatpak-info").exists() {
            "flatpak"
        } else {
            "unknown"
        },
        gstreamer_version: {
            let (a, b, c, d) = gstreamer::version();
            format!("{a}.{b}.{c}.{d}")
        },
        encoders: [
            "x264enc",
            "x265enc",
            "vah264enc",
            "vah265enc",
            "nvh264enc",
            "avenc_aac",
        ]
        .into_iter()
        .map(|name| (name, gstreamer::ElementFactory::find(name).is_some()))
        .collect(),
        events: snapshot(),
        logs: logs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect(),
        dropped_log_lines: DROPPED_LINES.load(std::sync::atomic::Ordering::Relaxed),
        dropped_stderr_bytes: DROPPED_STDERR_BYTES.load(std::sync::atomic::Ordering::Relaxed),
        settings: sanitized_settings(),
        devices: device_names(),
        recordings: recent_recordings(),
        platform: platform_info(),
        collection_status: serde_json::json!({
            "logs": "Last 5000 approved rec-prefixed or exact native-warning lines. Unknown, credential-bearing, invalid and oversized (>4KiB) lines are dropped and counted. Risky live transport text uses a fixed template. Asynchronous and process-local; no previous-process history.",
            "devices": "Names only; names containing recognizable serial labels, MAC addresses or by-id identifiers are omitted, not partially retained. Other suspicious names are omitted too. Filtering is best effort: unlabelled vendor serials or personal names may remain; review before sharing. No IDs, window titles or device paths. Enumeration failure is reported without error text.",
            "ffprobe": "Up to 5 newest regular mp4/mov/mkv files in the configured output directory (nonrecursive); numeric fields and known codec constants only. No filenames, tags or stderr.",
            "gpu": "DRM presence and encoder factories only; no active GPU/VA initialization or proof of hardware usability",
            "audio_outputs": "Not enumerated: current public capture API exposes microphone names only",
            "history": history_status(GUI_PROCESS.load(std::sync::atomic::Ordering::Relaxed))
        }),
        privacy: "Includes best-effort filtered application stderr. Arbitrary private text, paths or device names may remain: REVIEW BEFORE SHARING. Credential/URL-bearing lines are removed; environment stream keys are refreshed per captured line and app-configured keys are registered at use points. No recordings or environment dump. Encoder presence is not proof of usability.",
    }
}

const README: &str = "rec-rs local diagnostics (schema 1)\nFiltered log: only Rec's own messages are kept; known stream keys are removed. Review before sharing.\nUp to 5000 approved application/native-warning lines. Unknown, secret-bearing and oversized (>4KiB) lines are dropped and counted. Registered secrets include raw, URL-encoded, base64 and multiline segments of every length. Live transport details use a fixed template. Original stderr forwarding is best effort with bounded queue and counted drops. fd 2 remains blocking; the independent reader must run to drain its finite pipe (a paused reader cannot guarantee progress). Original destination O_NONBLOCK is preserved. Output may be lost at exit. Capture is asynchronous and process-local, not crash history. Includes typed events, filtered device names, numeric settings and probe summaries. Application text and device names may still be private: REVIEW BEFORE SHARING. No recordings collected.\n";

/// Export from the app process to include its ring. Run off the UI thread.
/// Creates a new mode-0600 tar; refuses existing files, including symlinks.
/// No temporary unredacted file exists. Failure may leave a partial private tar.
pub fn write_report(path: &Path) -> Result<()> {
    record(Event::ReportRequested);
    let bytes = serde_json::to_vec_pretty(&report())?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    let mut archive = tar::Builder::new(file);
    for (name, data) in [
        ("report.json", bytes.as_slice()),
        ("README.txt", README.as_bytes()),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o600);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        archive.append_data(&mut header, name, data)?;
    }
    archive.finish()?;
    archive.into_inner()?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn fd_tee_preserves_native_stderr_and_redacts_ring() {
        const CHILD: &str = "REC_DIAGNOSTICS_TEE_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let original_flags = unsafe { libc::fcntl(2, libc::F_GETFL) };
            assert!(original_flags >= 0);
            assert_eq!(
                unsafe { libc::fcntl(2, libc::F_SETFL, original_flags | libc::O_NONBLOCK) },
                0
            );
            install_stderr_tee().unwrap();
            assert_eq!(
                unsafe { libc::fcntl(2, libc::F_GETFL) } & libc::O_NONBLOCK,
                0
            );
            install_stderr_tee().unwrap(); // idempotent
            register_secret("configured-value-xyz");
            register_secret("multiline-part-one\nmultiline-part-two");
            {
                let secrets = secrets().lock().unwrap();
                for part in ["multiline-part-one", "multiline-part-two"] {
                    for variant in secret_variants(part) {
                        assert!(
                            filter_line(format!("rec: {variant}").as_bytes(), &secrets).is_none()
                        );
                    }
                }
            }
            // Simulate rotation after tee installation; old values stay protected.
            unsafe { std::env::set_var("REC_STREAM_KEY", "rotated-value-xyz") };
            let raw = b"native ready\nrtmp://host/private\nenv-value-abc\nconfigured-value-xyz\nrotated-value-xyz\n";
            assert_eq!(
                unsafe { libc::write(2, raw.as_ptr().cast(), raw.len()) },
                raw.len() as isize
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                let lines = logs().lock().unwrap().iter().cloned().collect::<Vec<_>>();
                if DROPPED_LINES.load(std::sync::atomic::Ordering::Relaxed) >= 5 {
                    assert!(lines.is_empty());
                    break;
                }
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            shutdown_tee();
            assert_ne!(
                unsafe { libc::fcntl(2, libc::F_GETFL) } & libc::O_NONBLOCK,
                0
            );
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "diagnostics::tests::fd_tee_preserves_native_stderr_and_redacts_ring",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("REC_STREAM_KEY", "env-value-abc")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(
            output.stderr,
            b"native ready\nrtmp://host/private\nenv-value-abc\nconfigured-value-xyz\nrotated-value-xyz\n"
        );
    }

    #[test]
    fn slow_stderr_does_not_block_native_writes() {
        const CHILD: &str = "REC_DIAGNOSTICS_SLOW_STDERR_CHILD";
        if std::env::var_os(CHILD).is_some() {
            install_stderr_tee().unwrap();
            assert_eq!(
                unsafe { libc::fcntl(2, libc::F_GETFL) } & libc::O_NONBLOCK,
                0
            );
            let bytes = [b'x'; 8192];
            for _ in 0..1280 {
                let n = unsafe { libc::write(2, bytes.as_ptr().cast(), bytes.len()) };
                assert_eq!(n, bytes.len() as isize);
            }
            shutdown_tee(); // also bounded when the original destination is full
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "diagnostics::tests::slow_stderr_does_not_block_native_writes",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // Deliberately never read the original stderr pipe.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("native stderr writer blocked on slow output");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn ring_contention_does_not_block_stderr_drain() {
        const CHILD: &str = "REC_DIAGNOSTICS_RING_CONTENTION_CHILD";
        if std::env::var_os(CHILD).is_some() {
            // Exercise real registration, including scrubbing already-retained
            // one/two-byte secrets, without contaminating parallel tests.
            for secret in ["Q", "Z7"] {
                let line = format!("rec: value {secret}");
                retain_line(line.clone());
                assert!(logs().lock().unwrap().contains(&line));
                register_secret(secret);
                assert!(!logs().lock().unwrap().contains(&line));
                let secrets = secrets().lock().unwrap();
                for variant in secret_variants(secret) {
                    assert!(filter_line(format!("rec: {variant}").as_bytes(), &secrets).is_none());
                }
            }
            install_stderr_tee().unwrap();
            let ring = logs().lock().unwrap();
            let before = DROPPED_LINES.load(std::sync::atomic::Ordering::Relaxed);
            // More than the finite fd pipe and both bounded queues can hold.
            // With REC_STREAM_KEY set, registration also waits for this ring
            // while holding secrets; neither wait may reach the fd drain.
            let bytes = b"rec: encoder ready\n".repeat(400);
            for _ in 0..1280 {
                assert_eq!(
                    unsafe { libc::write(2, bytes.as_ptr().cast(), bytes.len()) },
                    bytes.len() as isize
                );
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while DROPPED_LINES.load(std::sync::atomic::Ordering::Relaxed) == before {
                assert!(std::time::Instant::now() < deadline, "drops not counted");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(ring.len() <= CAPACITY);
            drop(ring);
            shutdown_tee();
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "diagnostics::tests::ring_contention_does_not_block_stderr_drain",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("REC_STREAM_KEY", "ring-contention-test-value")
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("stderr drain blocked on ring contention");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn history_distinguishes_gui_from_standalone() {
        assert!(history_status(true).starts_with("gui_process:"));
        assert!(history_status(false).starts_with("standalone_process:"));
    }

    #[test]
    fn credentials_are_removed_before_retention() {
        for value in [
            "stream_key=abc",
            "TOKEN: abc",
            "{\"password\":\"abc\"}",
            "Authorization: Bearer abc",
            "RTMPS://host/live/abc",
            "rtmp://user:pass@host/key",
            "https://host/?access_token=abc",
            "using private-configured-value now",
            "--stream-key abc",
            "secret: abc",
            "apiKey = abc",
        ] {
            let safe = redact(value.as_bytes(), Some("private-configured-value"));
            assert_eq!(safe, "[redacted credential/URL line]", "{value}");
            assert!(!safe.contains("abc"));
        }
        assert_eq!(redact(b"encoder ready", Some("")), "encoder ready");
        assert_eq!(redact(b"bad utf8 \xff", None), "bad utf8 \u{fffd}");
    }

    #[test]
    fn fail_closed_filter_and_secret_encodings() {
        assert!(filter_line(b"unknown private text", &[]).is_none());
        assert!(filter_line(b"GStreamer warning: arbitrary private text", &[]).is_none());
        for warning in NATIVE_WARNINGS {
            assert_eq!(
                filter_line(warning.as_bytes(), &[]).as_deref(),
                Some(*warning)
            );
        }
        assert_eq!(
            filter_line(b"rec: encoder ready", &[]).as_deref(),
            Some("rec: encoder ready")
        );
        assert_eq!(
            filter_line(b"rec: debug: rtmp connecting to private-host", &[]).as_deref(),
            Some("rec: live transport diagnostic (details omitted)")
        );
        for secret in ["a", "ab", "abc", "private +/?", "日本語"] {
            let variants = secret_variants(secret);
            for variant in &variants {
                assert!(
                    filter_line(format!("rec: {variant}").as_bytes(), &variants).is_none(),
                    "{variant}"
                );
            }
        }
        assert!(secret_variants("abc").contains(&"YWJj".into()));
        assert!(secret_variants("a").contains(&"YQ==".into()));
        assert!(secret_variants("a b").contains(&"a%20b".into()));
        assert!(filter_line(&vec![b'x'; MAX_LINE + 1], &[]).is_none());
        assert!(filter_line(b"rec: bad\xff", &[]).is_none());
        assert!(filter_line(b"rec: bad\rtext", &[]).is_none());
    }

    #[test]
    fn real_lines_across_chunks_and_eof() {
        let mut parser = Lines::default();
        let mut got = Vec::new();
        for chunk in [b"first\nsec".as_slice(), b"ond\n\nthird\r\nlast"] {
            parser.feed(chunk, |line| got.push(line.to_vec()));
        }
        parser.finish(|line| got.push(line.to_vec()));
        assert_eq!(
            got,
            [
                b"first".to_vec(),
                b"second".to_vec(),
                vec![],
                b"third\r".to_vec(),
                b"last".to_vec()
            ]
        );
        parser.finish(|_| panic!("duplicate EOF line"));
    }

    #[test]
    fn overlong_lines_are_discarded_whole_not_split() {
        let mut parser = Lines::default();
        let mut got = Vec::new();
        parser.feed(&vec![b'x'; MAX_LINE + 1], |_| panic!("not a line"));
        assert!(parser.pending.is_empty());
        parser.feed(b"secret-tail\nnormal\n", |s| got.push(s.to_vec()));
        assert_eq!(got, [b"normal".to_vec()]);
        parser.feed(&vec![b'x'; MAX_LINE], |s| got.push(s.to_vec()));
        parser.feed(b"\n", |s| got.push(s.to_vec()));
        assert_eq!(got[1].len(), MAX_LINE);
    }

    #[test]
    fn log_ring_is_bounded_and_writer_does_not_wait() {
        // Keep this test local to the actual ring; other tests may export it.
        for n in 0..6000 {
            retain_line(format!("rec: test line {n}"));
        }
        {
            let ring = logs().lock().unwrap();
            assert_eq!(ring.len(), CAPACITY);
            assert_eq!(ring.back().unwrap(), "rec: test line 5999");
            let before = DROPPED_LINES.load(std::sync::atomic::Ordering::Relaxed);
            retain_line("rec: must drop without waiting".into());
            assert!(DROPPED_LINES.load(std::sync::atomic::Ordering::Relaxed) > before);
        }
    }

    #[test]
    fn identifying_device_names_are_omitted() {
        for name in [
            "Webcam Serial ABC123",
            "Mic serial_number ABC123",
            "Monitor S/N ABC123",
            "Camera SN:ABC123",
            "Camera sn=ABC123",
            "USB Camera (aa:bb:cc:dd:ee:ff)",
            "Headset AA-BB-CC-DD-EE-FF",
            "bluez_input.AA_BB_CC_DD_EE_FF.0",
            "Headset aabb.ccdd.eeff",
            "/dev/v4l/by-id/usb-Camera_ABC123",
            "v4l-by-id-Camera_ABC123",
            "v4l_by_id_Camera_ABC123",
            "usb-Logitech_Webcam_ABC123-video-index0",
            "alsa_input.usb-Vendor_Mic_ABC123-00.mono-fallback",
            "alsa_output.usb-Vendor_ABC123",
        ] {
            assert!(safe_name(name).is_none(), "{name}");
        }
        for name in [
            "USB Microphone",
            "Logitech C920",
            "DELL U2720Q",
            "Built-in Audio",
            "Camera 1234",
            "日本語カメラ",
        ] {
            assert_eq!(safe_name(name).as_deref(), Some(name));
        }
        let result = names(Ok(vec![
            "USB Microphone".into(),
            "Camera Serial ABC123".into(),
            "Headset AA-BB-CC-DD-EE-FF".into(),
        ]));
        assert_eq!(result["names"], serde_json::json!(["USB Microphone"]));
        assert!(!result.to_string().contains("ABC123"));
    }

    #[test]
    fn untrusted_probe_and_names_are_filtered() {
        for s in [
            "/home/alice/camera",
            "https://secret",
            "token-123",
            "mic\nsecret",
            "password",
            "name=key",
        ] {
            assert!(safe_name(s).is_none());
        }
        assert_eq!(
            safe_name("USB Microphone").as_deref(),
            Some("USB Microphone")
        );
        let summary = probe_summary(&serde_json::json!({
            "format": {"filename": "/private/movie.mp4", "tags": {"secret": "key"}},
            "streams": [{"codec_name": "https://secret", "codec_type": "video", "width": 1920, "sample_rate": "48000", "tags": {"token": "key"}, "duration": "NaN"}]
        }));
        let text = summary.to_string();
        for secret in ["private", "secret", "tags", "filename", "NaN"] {
            assert!(!text.contains(secret));
        }
        assert_eq!(summary["streams"][0]["width"], 1920.0);
        assert_eq!(summary["streams"][0]["sample_rate"], 48000.0);
    }

    #[test]
    fn bounded_ordered_ring() {
        let mut ring = Ring::default();
        for _ in 0..6000 {
            ring.push(Event::RecordingStarted);
        }
        let entries = ring.snapshot();
        assert_eq!(entries.len(), CAPACITY);
        assert_eq!(entries[0].sequence, 1000);
        assert_eq!(entries[CAPACITY - 1].sequence, 5999);
    }

    #[test]
    fn arbitrary_environment_values_are_redacted() {
        for secret in [
            "rtmp://host/private-key",
            "/home/private",
            "token=secret",
            "wayland\nsecret",
        ] {
            assert_eq!(session_type(Some(secret)), "unknown");
        }
        assert_eq!(session_type(Some("wayland")), "wayland");
    }

    #[test]
    fn archive_is_private_allowlisted_and_no_clobber() {
        gstreamer::init().unwrap();
        let path = std::env::temp_dir().join(format!(
            "rec-diagnostics-{}-{}.tar",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_report(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_report(&path).is_err());
        let mut archive = tar::Archive::new(std::fs::File::open(&path).unwrap());
        let mut names = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            names.push(entry.path().unwrap().to_string_lossy().into_owned());
            let mut text = String::new();
            entry.read_to_string(&mut text).unwrap();
            if names.last().unwrap() == "report.json" {
                let json: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(json["schema_version"], 1);
                assert!(json["settings"]["fps"].is_number());
                assert!(json["settings"].get("stream_dests").is_none());
                assert!(json["settings"].get("output_dir").is_none());
                assert!(json.get("collection_status").is_some());
                assert!(json.get("environment").is_none());
            }
        }
        assert_eq!(names, ["report.json", "README.txt"]);
        std::fs::remove_file(path).unwrap();
    }
}
