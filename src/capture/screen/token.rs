//! The ScreenCast restore token.
//!
//! The portal can hand out a token with a session (`PersistMode::
//! ExplicitlyRevoked`). Passing it back to `SelectSources` restores the same
//! source and skips the picker, which is how the dialog is shown once instead
//! of on every recording — the closest Linux equivalent of macOS's one-time TCC
//! grant.
//!
//! Stored at `$XDG_STATE_HOME/rec-rs/portal-token` (fallback
//! `~/.local/state/rec-rs/`), mode 0600, written atomically: a token is a
//! capability, so a half-written file must never be handed back to the portal,
//! and an unreadable one must read as "no token" rather than fail a recording.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// `$XDG_STATE_HOME`, falling back to `~/.local/state` as the XDG spec says.
pub fn state_home() -> PathBuf {
    match std::env::var_os("XDG_STATE_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => crate::util::home_dir().join(".local/state"),
    }
}

/// The directory the token lives in.
pub fn dir() -> PathBuf {
    state_home().join("rec-rs")
}

/// The token's path.
pub fn path() -> PathBuf {
    dir().join("portal-token")
}

/// The stored token, or `None` when there is none (or it is unusable).
///
/// A corrupt file is reported once and treated as absent: the portal then
/// prompts, which is recoverable, whereas failing the recording is not.
pub fn load() -> Option<String> {
    load_from(&path())
}

/// Store `token`, replacing any previous one.
pub fn store(token: &str) -> Result<()> {
    store_to(&path(), token)
}

/// Forget the stored token (a revoked grant, or `permissions --request`).
pub fn clear() -> Result<()> {
    match std::fs::remove_file(path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("removing the portal restore token"),
    }
}

/// The validation both readers share: a token is a short, printable token.
///
/// Anything else is a truncated write, a different file by mistake or a
/// placeholder, and passing it to the portal only earns a confusing D-Bus
/// error.
pub fn valid(token: &str) -> bool {
    let t = token.trim();
    !t.is_empty()
        && t.len() <= 512
        && t.bytes()
            .all(|b| b.is_ascii_graphic() || b == b' ' || b == b'-' || b == b'_')
}

fn load_from(path: &Path) -> Option<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!(
                "rec: warning: cannot read the screen-capture grant at {}: {e}",
                path.display()
            );
            return None;
        }
    };
    let token = text.trim();
    if !valid(token) {
        eprintln!(
            "rec: warning: ignoring a corrupt screen-capture grant at {}; the portal will ask again",
            path.display()
        );
        return None;
    }
    Some(token.to_string())
}

fn store_to(path: &Path, token: &str) -> Result<()> {
    let token = token.trim();
    if !valid(token) {
        anyhow::bail!("refusing to store a malformed portal restore token");
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    create_private_dir(dir)?;
    // Write beside the target, then rename: a crash leaves either the old
    // token or the new one, never half of either.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode_private()
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(token.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(b"\n").ok();
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(dir) {
        if meta.is_dir() {
            return Ok(());
        }
        anyhow::bail!("{} exists and is not a directory", dir.display());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // The token is a capability: keep it out of other users' reach.
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

/// `OpenOptions` with mode 0600, for the token file.
trait ModePrivate {
    fn mode_private(&mut self) -> &mut Self;
}

impl ModePrivate for std::fs::OpenOptions {
    fn mode_private(&mut self) -> &mut Self {
        use std::os::unix::fs::OpenOptionsExt;
        self.mode(0o600)
    }
}

/// A temporary directory for the tests, removed on drop.
#[cfg(test)]
pub(crate) struct TempDir(pub PathBuf);

#[cfg(test)]
impl TempDir {
    pub fn new(name: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!(
            "rec-rs-screen-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_token_round_trips_and_keeps_its_file_private() {
        let dir = TempDir::new("token-roundtrip");
        let path = dir.0.join("portal-token");
        assert_eq!(load_from(&path), None, "no file means no token");
        store_to(&path, "  abc123  ").unwrap();
        assert_eq!(load_from(&path).as_deref(), Some("abc123"));
        // Replacing works and does not leave the temp file behind.
        store_to(&path, "def456").unwrap();
        assert_eq!(load_from(&path).as_deref(), Some("def456"));
        let entries: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the token is a capability");
        clear_at(&path);
        assert_eq!(load_from(&path), None);
    }

    fn clear_at(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn corruption_reads_as_no_token_and_is_never_stored() {
        let dir = TempDir::new("token-corrupt");
        let path = dir.0.join("portal-token");
        // Empty, whitespace only, binary and a too-long line: all unusable.
        for bad in ["", "   \n", "\u{0}\u{1}", &"x".repeat(600)] {
            std::fs::write(&path, bad).unwrap();
            assert_eq!(load_from(&path), None, "{bad:?}");
        }
        assert!(!valid("with\nnewline"));
        assert!(valid("a-normal-token_9"));
        // The writer refuses what the reader would reject.
        assert!(store_to(&path, "").is_err());
        assert!(store_to(&path, "bad\ttoken").is_err());
    }

    #[test]
    fn a_corrupt_file_does_not_stop_a_recording_but_warns() {
        // The warning goes to stderr; what matters is that this returns.
        let dir = TempDir::new("token-warn");
        let path = dir.0.join("portal-token");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(load_from(&path), None);
    }

    #[test]
    fn the_default_path_follows_xdg_state_home() {
        // `path()` reads the environment, so this only asserts the shape: the
        // real value depends on the environment running the test.
        let p = path();
        assert!(p.ends_with("rec-rs/portal-token"), "{}", p.display());
        let state = state_home();
        assert!(
            p.starts_with(&state),
            "{} vs {}",
            p.display(),
            state.display()
        );
        assert!(
            state.is_absolute() || state.starts_with("."),
            "{}",
            state.display()
        );
    }
}
