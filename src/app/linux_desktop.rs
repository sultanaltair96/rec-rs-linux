//! The small desktop actions upstream got from AppKit: open a URL, open a
//! folder, and show a file in the file manager.
//!
//! `xdg-open` opens URLs and folders. "Reveal" prefers the freedesktop
//! FileManager1 interface (`ShowItems`), which selects the file in the file
//! manager, and falls back to opening the folder when no file manager offers
//! it (a bare session, or inside a sandbox without `dbus-send`).
//!
//! Adapted from rec-rs src/app/macos.rs (MIT OR Apache-2.0), NSWorkspace
//! replaced by xdg-open and FileManager1.

use std::path::Path;
use std::process::{Command, Stdio};

/// A `file://` URI for `path`, percent-encoding everything outside the
/// unreserved set and `/` (RFC 3986).
pub fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*b as char)
            }
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `dbus-send` arguments that ask the file manager to select `file`.
/// `ShowItems` takes an array of URIs and a startup id (empty here).
pub fn reveal_args(file: &Path) -> Vec<String> {
    vec![
        "--session".into(),
        "--dest=org.freedesktop.FileManager1".into(),
        "--type=method_call".into(),
        "/org/freedesktop/FileManager1".into(),
        "org.freedesktop.FileManager1.ShowItems".into(),
        format!("array:string:{}", file_uri(file)),
        "string:".into(),
    ]
}

/// A desktop notification (the freedesktop notification service on the session
/// bus), fire-and-forget: a session without one keeps the message in the tray
/// area instead. Used when the app hides its own window for a recording.
pub fn notify(summary: &str, body: &str) {
    let (summary, body) = (summary.to_string(), body.to_string());
    std::thread::Builder::new()
        .name("rec-notify".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let Ok(rt) = rt else { return };
            rt.block_on(async move {
                let Ok(conn) = zbus::Connection::session().await else {
                    return;
                };
                let Ok(proxy) = zbus::Proxy::new(
                    &conn,
                    "org.freedesktop.Notifications",
                    "/org/freedesktop/Notifications",
                    "org.freedesktop.Notifications",
                )
                .await
                else {
                    return;
                };
                let _: Result<u32, _> = proxy
                    .call(
                        "Notify",
                        &(
                            "rec-rs",
                            0u32,
                            "",
                            summary,
                            body,
                            Vec::<String>::new(),
                            std::collections::HashMap::<String, zbus::zvariant::Value>::new(),
                            5000i32,
                        ),
                    )
                    .await;
            });
        })
        .ok();
}

/// Open a URL in the user's browser.
pub fn open_url(url: &str) {
    spawn_detached("xdg-open", &[url]);
}

/// Open a folder in the file manager.
pub fn open_folder(dir: &Path) {
    spawn_detached("xdg-open", &[&dir.to_string_lossy()]);
}

/// Show `file` in the file manager, selected if the file manager supports it.
pub fn reveal(file: &Path) {
    if file.exists() && dbus_send(&reveal_args(file)).is_some() {
        return;
    }
    // No FileManager1 (or no dbus-send): at least open where it is.
    if let Some(dir) = file.parent() {
        open_folder(dir);
    }
}

/// Run `dbus-send` with `args`; true when the call went through.
fn dbus_send(args: &[String]) -> Option<()> {
    let out = Command::new("dbus-send")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    out.success().then_some(())
}

fn spawn_detached(cmd: &str, args: &[&str]) {
    match Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // Reap without waiting: xdg-open can take a moment to exit.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => eprintln!("rec: could not run {cmd}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_are_escaped() {
        assert_eq!(
            file_uri(Path::new("/home/x/Videos/rec-1.mov")),
            "file:///home/x/Videos/rec-1.mov"
        );
        assert_eq!(
            file_uri(Path::new("/home/x/Videos/rec 1.mov")),
            "file:///home/x/Videos/rec%201.mov"
        );
        assert_eq!(
            file_uri(Path::new("/home/x/é#1.mov")),
            "file:///home/x/%C3%A9%231.mov"
        );
    }

    #[test]
    fn reveal_asks_file_manager1_for_the_file() {
        let a = reveal_args(Path::new("/home/x/Videos/rec-1.mov"));
        assert_eq!(a[0], "--session");
        assert_eq!(a[1], "--dest=org.freedesktop.FileManager1");
        assert!(a.contains(&"/org/freedesktop/FileManager1".to_string()));
        assert!(a.contains(&"org.freedesktop.FileManager1.ShowItems".to_string()));
        assert!(a.contains(&"array:string:file:///home/x/Videos/rec-1.mov".to_string()));
        assert_eq!(a.last().unwrap(), "string:", "an empty startup id");
    }
}
