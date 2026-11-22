//! Choosing a window from a `--window` selector, shared by the X11 and portal
//! paths.
//!
//! Upstream's rule (src/screen.rs, `find_window`): a selector that parses as a
//! window id picks that window; anything else is a case-insensitive substring
//! of `"<app> <title>"`, and the largest match wins, so `--window firefox`
//! records the browser window rather than its 1-pixel-tall tooltip.

use anyhow::{Result, anyhow, bail};

use crate::media_types::WindowInfo;

/// Find the window `sel` names in `windows`, or explain why it cannot.
///
/// Adapted from rec-rs src/screen.rs `find_window` (MIT OR Apache-2.0).
pub fn find<'a>(windows: &'a [WindowInfo], sel: &str) -> Result<&'a WindowInfo> {
    let sel = sel.trim();
    if sel.is_empty() {
        bail!("--window needs a window id or an app/title substring (see `rec-rs devices`)");
    }
    if let Ok(id) = sel.parse::<u64>() {
        return windows
            .iter()
            .find(|w| w.id == id)
            .ok_or_else(|| anyhow!("no on-screen window with id {id} (see `rec-rs devices`)"));
    }
    let lower = sel.to_lowercase();
    windows
        .iter()
        .filter(|w| {
            format!("{} {}", w.app, w.title)
                .to_lowercase()
                .contains(&lower)
        })
        .max_by(|a, b| area(a).total_cmp(&area(b)))
        .ok_or_else(|| anyhow!("no on-screen window matches {sel:?} (see `rec-rs devices`)"))
}

fn area(w: &WindowInfo) -> f64 {
    w.size.0 as f64 * w.size.1 as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(id: u64, app: &str, title: &str, size: (usize, usize)) -> WindowInfo {
        WindowInfo {
            id,
            app: app.to_string(),
            title: title.to_string(),
            size,
        }
    }

    fn sample() -> &'static [WindowInfo] {
        // Leaked once per test process: a `'static` list is the shortest way to
        // hand out references that outlive the call.
        static LIST: std::sync::OnceLock<Vec<WindowInfo>> = std::sync::OnceLock::new();
        LIST.get_or_init(build_sample)
    }

    fn build_sample() -> Vec<WindowInfo> {
        vec![
            w(1, "firefox", "Mozilla Firefox", (1280, 800)),
            w(2, "firefox", "Tooltip", (60, 20)),
            w(3, "foot", "zsh: ~/src", (800, 600)),
            w(4, "code", "rec-rs — Visual Studio Code", (1920, 1080)),
        ]
    }

    #[test]
    fn an_id_selects_exactly_that_window() {
        let found = find(sample(), "3").unwrap();
        assert_eq!(found.app, "foot");
        assert_eq!(found.id, 3);
    }

    #[test]
    fn a_substring_matches_app_or_title_and_prefers_the_largest() {
        let found = find(sample(), "firefox").unwrap();
        assert_eq!(found.id, 1, "the tooltip is smaller");
        // Title too, case-insensitively.
        assert_eq!(find(sample(), "VISUAL STUDIO").unwrap().id, 4);
        // App and title are concatenated, so a string spanning both matches.
        assert_eq!(find(sample(), "FOOT zsh").unwrap().id, 3);
        // Surrounding whitespace from a shell is ignored.
        assert_eq!(find(sample(), "  code  ").unwrap().id, 4);
    }

    #[test]
    fn misses_say_what_to_do() {
        let err = find(sample(), "999").unwrap_err().to_string();
        assert!(err.contains("no on-screen window with id 999"), "{err}");
        assert!(err.contains("rec-rs devices"), "{err}");
        let err = find(sample(), "nothing-like-it").unwrap_err().to_string();
        assert!(err.contains("no on-screen window matches"), "{err}");
        assert!(find(sample(), "   ").is_err());
    }

    #[test]
    fn an_empty_list_still_reports_a_useful_error() {
        assert!(find(&[], "1").is_err());
        assert!(find(&[], "firefox").is_err());
    }
}
