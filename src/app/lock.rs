//! Screen lock (G8): the preview stops converting frames while the session is
//! locked, as upstream does ("nothing is drawn while the window is hidden or
//! the screen is locked", and its measured cost fell from about a core to 2 %).
//!
//! Two D-Bus sources, either can say "locked":
//! - `org.freedesktop.login1`'s `Session.LockedHint` on the **system** bus
//!   (systemd-logind, the authoritative hint on most desktops);
//! - the screen saver's `ActiveChanged` signal on the **session** bus
//!   (`org.freedesktop.ScreenSaver`, which GNOME and KDE both implement), as a
//!   fallback for sessions without logind.
//!
//! They run on their own thread with a small tokio runtime; a change wakes the
//! app's event loop, and the app folds the state into `preview::Tap::set_wanted`.
//! A session with neither bus (or neither service) simply never reports locked:
//! the window's occlusion state is then the only signal, as before.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::StreamExt;

/// Whether the session counts as locked: either source saying so is enough,
/// and a source that is not available says nothing.
pub fn locked_from(logind_hint: Option<bool>, screensaver_active: Option<bool>) -> bool {
    logind_hint.unwrap_or(false) || screensaver_active.unwrap_or(false)
}

/// Whether preview frames are wanted: the window is visible, not covered, and
/// the screen is not locked.
pub fn preview_wanted(visible: bool, occluded: bool, locked: bool) -> bool {
    visible && !occluded && !locked
}

/// A subscription to the lock state. Dropping it ends the worker thread.
pub struct LockWatcher {
    locked: Arc<AtomicBool>,
    /// Kept for the join; the thread also ends when its `wake` is dropped.
    _thread: std::thread::JoinHandle<()>,
}

impl LockWatcher {
    /// Start watching. `wake` requests a redraw when the state changes (it runs
    /// on the watcher's thread and must not block). Returns `None` when the
    /// runtime or both buses are unavailable, in which case the app keeps its
    /// occlusion-only behaviour.
    pub fn start(wake: Arc<dyn Fn() + Send + Sync>) -> Option<LockWatcher> {
        let locked = Arc::new(AtomicBool::new(false));
        let rx = locked.clone();
        let thread = std::thread::Builder::new()
            .name("rec-screen-lock".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let Ok(rt) = rt else { return };
                rt.block_on(async move {
                    let logind = Arc::new(AtomicBool::new(false));
                    let saver = Arc::new(AtomicBool::new(false));
                    let publish = {
                        let rx = rx.clone();
                        let (logind, saver) = (logind.clone(), saver.clone());
                        let wake = wake.clone();
                        move || {
                            let now = locked_from(
                                Some(logind.load(Ordering::Relaxed)),
                                Some(saver.load(Ordering::Relaxed)),
                            );
                            if rx.swap(now, Ordering::Relaxed) != now {
                                wake();
                            }
                        }
                    };
                    let publish = Arc::new(publish);
                    // Both sources run until their bus goes away; one missing
                    // service only leaves its flag false.
                    let a = watch_logind(logind, publish.clone());
                    let b = watch_screensaver(saver, publish);
                    tokio::join!(a, b);
                });
            })
            .ok()?;
        Some(LockWatcher {
            locked,
            _thread: thread,
        })
    }

    /// Whether the session is locked right now.
    pub fn locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }
}

type Publish = Arc<dyn Fn() + Send + Sync>;

/// `Session.LockedHint` from systemd-logind on the system bus.
async fn watch_logind(flag: Arc<AtomicBool>, publish: Publish) {
    let Ok(conn) = zbus::Connection::system().await else {
        return;
    };
    let Ok(proxy) = zbus::Proxy::new(
        &conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1/session/auto",
        "org.freedesktop.login1.Session",
    )
    .await
    else {
        return;
    };
    if let Ok(hint) = proxy.get_property::<bool>("LockedHint").await {
        flag.store(hint, Ordering::Relaxed);
        publish();
    }
    let Ok(mut changed) = proxy.receive_signal("LockedHint").await else {
        return;
    };
    while let Some(msg) = changed.next().await {
        if let Ok(hint) = msg.body().deserialize::<bool>() {
            flag.store(hint, Ordering::Relaxed);
            (publish)();
        }
    }
}

/// The screen saver's `ActiveChanged` on the session bus, as a broadcast match
/// rule so GNOME's, KDE's and any other implementation of the interface count.
async fn watch_screensaver(flag: Arc<AtomicBool>, publish: Publish) {
    let Ok(conn) = zbus::Connection::session().await else {
        return;
    };
    // The initial state, from whichever service owns the interface.
    for (service, path) in [
        (
            "org.freedesktop.ScreenSaver",
            "/org/freedesktop/ScreenSaver",
        ),
        ("org.gnome.ScreenSaver", "/org/gnome/ScreenSaver"),
    ] {
        let Ok(proxy) = zbus::Proxy::new(&conn, service, path, "org.freedesktop.ScreenSaver").await
        else {
            continue;
        };
        if let Ok(active) = proxy.get_property::<bool>("Active").await {
            flag.store(active, Ordering::Relaxed);
            (publish)();
            break;
        }
    }
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal);
    let Ok(rule) = rule.interface("org.freedesktop.ScreenSaver") else {
        return;
    };
    let Ok(rule) = rule.member("ActiveChanged") else {
        return;
    };
    let Ok(mut stream) = zbus::MessageStream::for_match_rule(rule.build(), &conn, Some(16)).await
    else {
        return;
    };
    while let Some(Ok(msg)) = stream.next().await {
        if let Ok(active) = msg.body().deserialize::<bool>() {
            flag.store(active, Ordering::Relaxed);
            (publish)();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn either_source_can_lock_it() {
        // Upstream's rule: draw nothing while the screen is locked. logind's
        // hint and the screen saver both count; unavailable sources say nothing.
        assert!(!locked_from(None, None));
        assert!(!locked_from(Some(false), Some(false)));
        assert!(locked_from(Some(true), None));
        assert!(locked_from(None, Some(true)));
        assert!(locked_from(Some(false), Some(true)));
        assert!(locked_from(Some(true), Some(false)));
    }

    #[test]
    fn preview_frames_stop_when_locked_covered_or_hidden() {
        assert!(preview_wanted(true, false, false));
        assert!(!preview_wanted(true, false, true), "locked");
        assert!(!preview_wanted(true, true, false), "covered");
        assert!(!preview_wanted(false, false, false), "hidden");
    }
}
