//! Disk admission and monitoring, called outside the writer lock/hot path.
//! Available (not root-reserved) blocks count. Admission and the running
//! low watermark are separate: reserve room for finalization before starting,
//! then stop cleanly before the filesystem fills.
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

pub const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const MIN_FREE: u64 = 2_000_000_000;

const MB: u64 = 1_000_000;
// mp4's bounded movie-index reader accepts 128 MiB. Allow four copies plus
// an in-flight fragment and the monitor's five-second observation delay.
const MAX_INDEX_BYTES: u64 = 128 * 1024 * 1024;
const MAX_BITRATE: u64 = 80_000_000 + 3 * 320_000;

#[derive(Debug, PartialEq, Eq)]
pub struct Thresholds {
    pub admission: u64,
    pub low_water: u64,
}

fn parse_mb(name: &str, value: Option<&str>) -> Result<Option<u64>> {
    value
        .map(|value| {
            value
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0 || name == "REC_LOW_WATER_MB")
                .and_then(|n| n.checked_mul(MB))
                .with_context(|| {
                    format!("{name} must be a positive whole number of MB (1 MB = 1000000 bytes)")
                })
        })
        .transpose()
}

fn thresholds(
    bitrate: u64,
    fragment_secs: u64,
    min: Option<&str>,
    low: Option<&str>,
) -> Result<Thresholds> {
    let fragment_reserve = u128::from(bitrate.max(MAX_BITRATE))
        .saturating_mul(u128::from(fragment_secs) + u128::from(CHECK_INTERVAL.as_secs()))
        .div_ceil(8)
        .min(u128::from(u64::MAX)) as u64;
    let default_low = 1_000_000_000u64.max((4 * MAX_INDEX_BYTES).saturating_add(fragment_reserve));
    let low_water = parse_mb("REC_LOW_WATER_MB", low)?.unwrap_or(default_low);
    let admission = parse_mb("REC_MIN_FREE_MB", min)?.unwrap_or_else(|| floor(bitrate));
    Ok(Thresholds {
        admission: admission.max(low_water),
        low_water,
    })
}

fn environment(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {name}")),
    }
}

pub fn check_interval() -> Result<std::time::Duration> {
    match environment("REC_DISK_CHECK_MS")? {
        None => Ok(CHECK_INTERVAL),
        Some(value) => {
            let ms = value
                .parse::<u64>()
                .ok()
                .filter(|n| *n >= 10)
                .context("REC_DISK_CHECK_MS must be an integer of at least 10 milliseconds")?;
            Ok(std::time::Duration::from_millis(ms))
        }
    }
}

pub fn floor(bitrate: u64) -> u64 {
    MIN_FREE.max(((u128::from(bitrate) * 300).div_ceil(8)).min(u128::from(u64::MAX)) as u64)
}

fn check_available(available: u64, required: u64) -> Result<()> {
    if available < required {
        bail!(
            "low disk space: {available} bytes available, {required} bytes required; stopping recording to finalize safely"
        );
    }
    Ok(())
}

pub fn check(path: &Path, required: u64) -> Result<()> {
    let path_c =
        CString::new(path.as_os_str().as_bytes()).context("output directory contains NUL")?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: path is NUL terminated and stats points to writable storage.
    if unsafe { libc::statvfs(path_c.as_ptr(), stats.as_mut_ptr()) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            bail!(
                "output directory disappeared; an unlinked recording may be unrecoverable ({error})"
            );
        }
        return Err(error).context("checking output filesystem free space");
    }
    // SAFETY: successful statvfs initialized stats.
    let stats = unsafe { stats.assume_init() };
    let available = stats.f_bavail.saturating_mul(stats.f_frsize);
    check_available(available, required)
}

/// Create the destination directory before testing its actual filesystem.
/// Live-only sessions must not call this function.
pub fn preflight(output: &Path, bitrate: u64) -> Result<(std::path::PathBuf, u64)> {
    let directory = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(directory).context("creating output directory for disk preflight")?;
    let min = environment("REC_MIN_FREE_MB")?;
    let low = environment("REC_LOW_WATER_MB")?;
    // Match the muxer's fragment override, including its default on bad input.
    let fragment_secs = std::env::var("REC_FRAGMENT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(2000)
        .div_ceil(1000); // qtmux fragment-duration is milliseconds despite the variable name.
    let limits = thresholds(bitrate, fragment_secs, min.as_deref(), low.as_deref())?;
    check(directory, limits.admission)?;
    Ok((directory.to_path_buf(), limits.low_water))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_and_defaults() {
        let limits = thresholds(8_000_000, 2, None, None).unwrap();
        assert_eq!(limits.admission, MIN_FREE);
        assert_eq!(limits.low_water, 1_000_000_000);
        assert!(limits.low_water >= 4 * MAX_INDEX_BYTES + MAX_BITRATE * 2 / 8);
        assert_eq!(
            thresholds(80_000_000, 2, None, None).unwrap().admission,
            3_000_000_000
        );
        assert_eq!(
            thresholds(0, 2, Some("100"), Some("20")).unwrap(),
            Thresholds {
                admission: 100 * MB,
                low_water: 20 * MB
            }
        );
        assert_eq!(
            thresholds(0, 2, Some("10"), Some("20")).unwrap().admission,
            20 * MB
        );
        assert!(thresholds(0, 600, None, None).unwrap().low_water > MIN_FREE);
        assert_eq!(
            thresholds(u64::MAX, u64::MAX, None, None)
                .unwrap()
                .low_water,
            u64::MAX
        );
    }

    #[test]
    fn invalid_settings_fail_explicitly() {
        for value in ["", "-1", "1.5", "garbage", "18446744073709551615"] {
            for name in ["REC_MIN_FREE_MB", "REC_LOW_WATER_MB"] {
                assert!(
                    parse_mb(name, Some(value))
                        .unwrap_err()
                        .to_string()
                        .contains(name)
                );
            }
        }
        assert!(parse_mb("REC_MIN_FREE_MB", Some("0")).is_err());
        assert_eq!(parse_mb("REC_LOW_WATER_MB", Some("0")).unwrap(), Some(0));
        assert_eq!(parse_mb("REC_MIN_FREE_MB", None).unwrap(), None);
    }

    #[test]
    fn reserve_scales_with_bitrate() {
        assert_eq!(floor(0), MIN_FREE);
        assert_eq!(floor(8_000_000), MIN_FREE);
        assert_eq!(floor(80_000_000), 3_000_000_000);
        assert_eq!(floor(u64::MAX), u64::MAX);
    }

    #[test]
    fn low_water_boundary() {
        assert!(check_available(MIN_FREE - 1, MIN_FREE).is_err());
        assert!(check_available(MIN_FREE, MIN_FREE).is_ok());
        assert!(check_available(MIN_FREE + 1, MIN_FREE).is_ok());
    }

    #[test]
    fn rejected_preflight_does_not_create_recording() {
        let output = std::env::temp_dir().join(format!("rec-disk-{}.mp4", std::process::id()));
        assert!(!output.exists());
        assert!(preflight(&output, u64::MAX).is_err());
        assert!(!output.exists());
    }

    #[test]
    fn statvfs_reports_errors_and_reads_real_filesystem() {
        assert!(check(Path::new("."), 0).is_ok());
        assert!(check(Path::new("."), u64::MAX).is_err());
        assert!(check(Path::new("/nonexistent-rec-rs-disk-test/path"), 0).is_err());
    }
}
