//! Stream keys live in the Secret Service (the freedesktop keyring: GNOME
//! Keyring, KWallet's service, KeePassXC's, …), keyed per destination, never in
//! the settings file, a plain file or a log.
//!
//! `$REC_STREAM_KEY` overrides the stored key for one run (the self-test uses
//! it). A box with no Secret Service gets a clear error instead of a silent
//! fallback: a stream key is a password.
//!
//! The store sits behind [`Store`] so the adapter can be tested without a
//! session bus, and [`Memory`] is that test backend.
//!
//! Adapted from rec-rs src/live/keychain.rs (MIT OR Apache-2.0): the macOS
//! Keychain became the freedesktop Secret Service.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Result, anyhow};

/// The service name every key is stored under.
pub const SERVICE: &str = "rec-rs stream key";

/// Where stream keys are kept.
pub trait Store: Send + Sync {
    /// The stored key, or None when there is none for `account`.
    fn get(&self, account: &str) -> Option<String>;
    fn set(&self, account: &str, key: &str) -> Result<()>;
    fn delete(&self, account: &str) -> Result<()>;
}

/// Account name for a destination (service id or server URL), like upstream's
/// Keychain account.
pub fn account(dest: &str) -> String {
    super::register_url_secrets(dest);
    dest.trim().trim_end_matches('/').to_string()
}

/// The freedesktop Secret Service, through the `keyring` crate.
pub struct SecretService;

impl Store for SecretService {
    fn get(&self, account: &str) -> Option<String> {
        super::register_url_secrets(account);
        let entry = keyring::Entry::new(SERVICE, account).ok()?;
        match entry.get_password() {
            Ok(key) => {
                crate::diagnostics::register_secret(&key);
                Some(key)
            }
            Err(keyring::Error::NoEntry) => None,
            Err(e) => {
                if std::env::var_os("REC_DEBUG").is_some() {
                    eprintln!("rec: debug: reading the stream key for {account}: {e}");
                }
                None
            }
        }
    }

    fn set(&self, account: &str, key: &str) -> Result<()> {
        super::register_url_secrets(account);
        crate::diagnostics::register_secret(key);
        let entry = keyring::Entry::new(SERVICE, account).map_err(|e| {
            anyhow!(
                "the Secret Service is not available ({e}); start a keyring daemon \
                 (gnome-keyring-daemon) or set REC_STREAM_KEY. Stream keys are never written to a file"
            )
        })?;
        entry.set_password(key).map_err(|e| {
            anyhow!(
                "the Secret Service refused the key ({e}); start a keyring daemon \
                 (gnome-keyring-daemon) or set REC_STREAM_KEY"
            )
        })
    }

    fn delete(&self, account: &str) -> Result<()> {
        super::register_url_secrets(account);
        let entry = keyring::Entry::new(SERVICE, account).map_err(|e| {
            anyhow!("the Secret Service is not available ({e}); nothing was deleted")
        })?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow!(
                "the Secret Service refused to delete the key ({e})"
            )),
        }
    }
}

/// In-memory backend: the adapter's tests, and nothing else (a key in memory is
/// still not a key on disk).
#[derive(Default)]
#[allow(dead_code)]
pub struct Memory(Mutex<HashMap<String, String>>);

impl Store for Memory {
    fn get(&self, account: &str) -> Option<String> {
        let key = self.0.lock().unwrap().get(account).cloned();
        if let Some(key) = &key {
            crate::diagnostics::register_secret(key);
        }
        key
    }

    fn set(&self, account: &str, key: &str) -> Result<()> {
        crate::diagnostics::register_secret(key);
        if key.is_empty() {
            return Err(anyhow!("the stream key is empty"));
        }
        self.0
            .lock()
            .unwrap()
            .insert(account.to_string(), key.to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<()> {
        self.0.lock().unwrap().remove(account);
        Ok(())
    }
}

fn store() -> SecretService {
    SecretService
}

/// The stored key for a destination, if any.
pub fn get(dest: &str) -> Option<String> {
    store().get(&account(dest))
}

/// Store (or replace) the key for a destination.
pub fn set(dest: &str, key: &str) -> Result<()> {
    store().set(&account(dest), key)
}

/// Remove the stored key for a destination (no key is not an error).
pub fn delete(dest: &str) -> Result<()> {
    store().delete(&account(dest))
}

/// True when a Secret Service answers. Used by `rec-rs permissions` and a
/// clearer error on a headless box.
pub fn available() -> bool {
    match keyring::Entry::new(SERVICE, "rec-rs availability probe") {
        Ok(e) => !matches!(
            e.get_password(),
            Err(keyring::Error::PlatformFailure(_)) | Err(keyring::Error::NoStorageAccess(_))
        ),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The adapter round-trips through its store, and a destination is keyed by
    /// its normalised name (so `youtube` and a trailing slash agree).
    #[test]
    fn round_trip() {
        let store = Memory::default();
        assert_eq!(store.get("youtube"), None);
        store.set("youtube", "sk_live_123").unwrap();
        assert_eq!(store.get("youtube").as_deref(), Some("sk_live_123"));
        store.set("youtube", "sk_live_456").unwrap();
        assert_eq!(store.get("youtube").as_deref(), Some("sk_live_456"));
        store.delete("youtube").unwrap();
        assert_eq!(store.get("youtube"), None);
        // Deleting a key that is not there is not an error.
        store.delete("youtube").unwrap();
        assert!(store.set("youtube", "").is_err(), "an empty key is refused");
        // Destinations are normalised before they become account names.
        assert_eq!(account("  rtmp://h/live/  "), "rtmp://h/live");
        assert_eq!(account("youtube"), "youtube");
    }

    /// The real Secret Service, when the machine has one. Set
    /// `REC_KEYRING_TEST=1` to require it (the self-test does inside a
    /// dbus-run-session); without it the test reports and passes, so a headless
    /// CI box without a keyring stays green.
    #[test]
    fn secret_service_round_trip() {
        if !available() {
            if std::env::var_os("REC_KEYRING_TEST").is_some() {
                panic!("REC_KEYRING_TEST is set but no Secret Service answers");
            }
            eprintln!("skipping: no Secret Service is running in this session");
            return;
        }
        let dest = format!("rec-rs-test-{}", std::process::id());
        set(&dest, "sk_test_123").unwrap();
        assert_eq!(get(&dest).as_deref(), Some("sk_test_123"));
        set(&dest, "sk_test_456").unwrap();
        assert_eq!(get(&dest).as_deref(), Some("sk_test_456"));
        delete(&dest).unwrap();
        assert_eq!(get(&dest), None);
    }
}
