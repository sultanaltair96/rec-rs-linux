//! "Check for Updates…": ask GitHub Releases for the newest Rec release and,
//! when it is newer than this build, offer its download page.
//!
//! Upstream lets Sparkle replace the bundle. On Linux the Flatpak is updated by
//! Flatpak (the app hides this item there, see [`crate::app::install`]), and an
//! AppImage or distro build points at the release page. The network call and
//! the comparison are separate functions so the comparison is unit tested
//! without a network, and the call runs on a worker thread: the window never
//! waits for GitHub.
//!
//! Adapted from rec-rs src/app/updater.rs (MIT OR Apache-2.0), Sparkle
//! replaced by the GitHub Releases API.

use anyhow::{Context, Result, bail};

/// The repository's newest release, over the public API.
pub const LATEST_API: &str =
    "https://api.github.com/repos/sultanaltair96/rec-rs-linux/releases/latest";
/// The human page the update item opens (the repo's releases).
pub const DOWNLOAD_PAGE: &str = "https://github.com/sultanaltair96/rec-rs-linux/releases/latest";

/// This build's version.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The parts of a release the app needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// `v0.2.0`, as tagged.
    pub tag: String,
    pub name: String,
    /// Where to send the user: the release's page, else the releases page.
    pub url: String,
}

/// Parse the GitHub "latest release" JSON. Unknown fields are ignored: the
/// API adds fields over time, and a missing `html_url` falls back to the
/// releases page rather than failing the check.
pub fn parse_release(json: &str) -> Option<Release> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let tag = v.get("tag_name")?.as_str()?.trim().to_string();
    if tag.is_empty() {
        return None;
    }
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or(&tag)
        .trim()
        .to_string();
    let url = v
        .get("html_url")
        .and_then(|u| u.as_str())
        .filter(|u| u.starts_with("https://"))
        .unwrap_or(DOWNLOAD_PAGE)
        .to_string();
    Some(Release { tag, name, url })
}

/// A version tag as numbers: `v1.2.3`, `1.2`, `1.2.3-beta.1` (the pre-release
/// suffix is ignored: it still counts as 1.2.3). `None` when it isn't one.
pub fn version_of(tag: &str) -> Option<(u64, u64, u64)> {
    let t = tag.trim().trim_start_matches(['v', 'V']);
    let core = t.split(['-', '+']).next().unwrap_or(t);
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().unwrap_or("0").parse().unwrap_or(0);
    let patch = it.next().unwrap_or("0").parse().unwrap_or(0);
    Some((major, minor, patch))
}

/// Whether `remote` is a later release than `local` (both tags or versions).
pub fn newer(remote: &str, local: &str) -> bool {
    match (version_of(remote), version_of(local)) {
        (Some(r), Some(l)) => r > l,
        // An unparsable remote tag is not an update; an unparsable local one
        // (a `cargo build` from a branch) is newer unless equal.
        (Some(_), None) => true,
        _ => false,
    }
}

/// Read the newest release from GitHub. Only `https://` URLs are followed.
pub fn fetch(url: &str) -> Result<String> {
    if !url.starts_with("https://") {
        bail!("refusing a non-HTTPS update check: {url}");
    }
    let mut resp = ureq::get(url)
        .header("User-Agent", concat!("rec-rs/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .call()
        .with_context(|| format!("could not reach {url}"))?;
    let body = resp
        .body_mut()
        .read_to_string()
        .context("could not read the answer")?;
    Ok(body)
}

/// What a check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    /// A newer release to offer.
    Newer(Release),
    /// Already up to date (the newest release's tag).
    Current(String),
}

/// Compare the newest release with `local`, fetching from `url` with `get`.
pub fn check_with(
    local: &str,
    url: &str,
    get: impl FnOnce(&str) -> Result<String>,
) -> Result<Found> {
    let release = parse_release(&get(url)?)
        .with_context(|| format!("{url} did not answer with a release"))?;
    if newer(&release.tag, local) {
        Ok(Found::Newer(release))
    } else {
        Ok(Found::Current(release.tag))
    }
}

/// [`check_with`] against GitHub, for this build.
pub fn check_now() -> Result<Found> {
    check_with(current_version(), LATEST_API, fetch)
}

/// The line the app shows for a result.
pub fn message(found: &Found) -> String {
    match found {
        Found::Newer(r) => format!(
            "Rec {} is available (this is {}).",
            r.tag,
            current_version()
        ),
        Found::Current(tag) => format!("Rec is up to date (the newest release is {tag})."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{
        "url": "https://api.github.com/repos/x/y/releases/1",
        "tag_name": "v0.2.0",
        "name": "Rec 0.2.0",
        "html_url": "https://github.com/sultanaltair96/rec-rs-linux/releases/tag/v0.2.0",
        "prerelease": false,
        "assets": []
    }"#;

    #[test]
    fn parses_a_release() {
        let r = parse_release(JSON).unwrap();
        assert_eq!(r.tag, "v0.2.0");
        assert_eq!(r.name, "Rec 0.2.0");
        assert!(r.url.ends_with("/releases/tag/v0.2.0"));
        // A release without a page falls back to the releases list.
        let r = parse_release(r#"{"tag_name": "v1.0.0"}"#).unwrap();
        assert_eq!(r.url, DOWNLOAD_PAGE);
        assert_eq!(r.name, "v1.0.0");
        // Errors and odd answers are just "no release".
        assert_eq!(parse_release("not json"), None);
        assert_eq!(parse_release(r#"{"message": "Not Found"}"#), None);
        assert_eq!(parse_release(r#"{"tag_name": "  "}"#), None);
    }

    #[test]
    fn versions_compare_numerically() {
        assert_eq!(version_of("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(version_of("1.2"), Some((1, 2, 0)));
        assert_eq!(version_of("1"), Some((1, 0, 0)));
        assert_eq!(version_of("v0.2.0-beta.1"), Some((0, 2, 0)));
        assert_eq!(version_of("nightly"), None);
        assert!(newer("v0.2.0", "0.1.9"));
        assert!(newer("v0.10.0", "0.9.9"), "not string order");
        assert!(!newer("v0.1.0", "0.1.0"));
        assert!(!newer("v0.0.9", "0.1.0"));
        assert!(!newer("nightly", "0.1.0"), "an odd tag is not an update");
    }

    #[test]
    fn checks_without_a_network() {
        let found = check_with("0.1.0", LATEST_API, |_| Ok(JSON.to_string())).unwrap();
        assert!(matches!(found, Found::Newer(_)));
        assert!(message(&found).contains("v0.2.0"));
        let found = check_with("0.2.0", LATEST_API, |_| Ok(JSON.to_string())).unwrap();
        assert_eq!(found, Found::Current("v0.2.0".into()));
        assert!(message(&found).contains("up to date"));
        let err = check_with("0.2.0", LATEST_API, |_| bail!("no network"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no network"), "{err}");
        let err = check_with("0.2.0", LATEST_API, |_| Ok("{}".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not answer with a release"), "{err}");
        // Plain HTTP is refused (the API is HTTPS-only).
        assert!(fetch("http://example.com/x").is_err());
    }
}
