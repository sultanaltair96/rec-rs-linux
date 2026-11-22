//! Small shared helpers: clocks, display sizes, XDG paths.
//!
//! Adapted from rec-rs src/util.rs and src/settings.rs (MIT OR Apache-2.0),
//! with the Objective-C helpers removed.

use std::path::PathBuf;

/// `x` rounded to the nearest even number, at least 2.
///
/// Output sizes are even because every encoder wants it: H.264/HEVC 4:2:0
/// chroma is shared between pixel pairs.
pub fn even(x: f64) -> usize {
    ((x / 2.0).round() as usize * 2).max(2)
}

/// `mmm:ss` or `h:mm:ss`, for the live status line.
pub fn clock(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// A linear peak as a right-aligned dBFS string, `-inf` for silence.
pub fn db(peak: f32) -> String {
    if peak <= 1e-5 {
        "  -inf".to_string()
    } else {
        format!("{:>6.1}", 20.0 * peak.log10())
    }
}

/// `PREFIX-YYYY-MM-DD-HHMMSS.EXT` in local time, for recordings and replays.
pub fn timestamped(prefix: &str, ext: &str) -> String {
    let mut buf = [0u8; 64];
    let n = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        libc::strftime(
            buf.as_mut_ptr() as *mut _,
            buf.len(),
            c"%Y-%m-%d-%H%M%S".as_ptr(),
            &tm,
        )
    };
    format!("{prefix}-{}.{ext}", String::from_utf8_lossy(&buf[..n]))
}

/// `$HOME`, or empty when there is none (a daemon without a home directory).
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// `$XDG_CONFIG_HOME`, falling back to `$HOME/.config` as the XDG spec says.
pub fn config_home() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home_dir().join(".config"),
    }
}

/// The folder recordings default to: `$XDG_VIDEOS_DIR`, else `~/Videos`.
///
/// `REC_OUTPUT_DIR` overrides it, which is how the self-tests and CI keep
/// recordings out of a real Videos folder.
pub fn videos_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("REC_OUTPUT_DIR") {
        return PathBuf::from(p);
    }
    match std::env::var_os("XDG_VIDEOS_DIR") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home_dir().join("Videos"),
    }
}

/// Where runtime state (the pidfile) goes: `$XDG_RUNTIME_DIR`, else
/// `/tmp/rec-rs-$UID`.
///
/// The fallback is created with mode 0700, because the pidfile names the
/// recording's output path and must not be readable by other users.
pub fn runtime_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(p);
        if !p.as_os_str().is_empty() && p.is_dir() {
            return p;
        }
    }
    let dir = std::env::temp_dir().join(format!("rec-rs-{}", unsafe { libc::getuid() }));
    create_private_dir(&dir);
    dir
}

/// Create `dir` (and parents) readable only by the current user.
fn create_private_dir(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(dir) {
        if meta.is_dir() {
            return;
        }
    }
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_sizes() {
        assert_eq!(even(1080.0), 1080);
        assert_eq!(even(1079.4), 1080);
        assert_eq!(even(1077.0), 1078);
        assert_eq!(even(0.0), 2);
        assert_eq!(even(1.0), 2);
    }

    #[test]
    fn clock_formatting() {
        assert_eq!(clock(0.0), "00:00");
        assert_eq!(clock(59.9), "00:59");
        assert_eq!(clock(61.0), "01:01");
        assert_eq!(clock(3600.0), "1:00:00");
        assert_eq!(clock(3661.0), "1:01:01");
        assert_eq!(clock(-5.0), "00:00");
    }

    #[test]
    fn db_formatting() {
        assert_eq!(db(0.0), "  -inf");
        assert_eq!(db(1.0), "   0.0");
        assert_eq!(db(0.5), "  -6.0");
    }

    #[test]
    fn timestamps_are_safe_file_names() {
        let s = timestamped("rec", "mov");
        assert!(s.starts_with("rec-") && s.ends_with(".mov"), "{s}");
        assert!(!s.contains('/') && !s.contains(' '), "{s}");
        assert_eq!(
            s.len(),
            "rec-".len() + 17 + ".mov".len(),
            "YYYY-MM-DD-HHMMSS"
        );
    }
}
