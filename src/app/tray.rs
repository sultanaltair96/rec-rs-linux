//! The tray item: Rec's replacement for upstream's macOS menu bar item.
//!
//! A StatusNotifierItem with the same menu, a red dot while recording (plus the
//! elapsed time in its title and tooltip) and "LIVE" while streaming without a
//! recording. SNI hosts differ in what they show: KDE and the GNOME AppIndicator
//! extension draw the icon and the title next to it, while some hosts show only
//! the icon — the tooltip carries the same text everywhere, so the state is
//! never invisible.
//!
//! The menu model, the title/tooltip mapping and the icon are pure functions
//! (unit tested). [`start`] confirms registration; [`Watcher`] listens for
//! NameOwnerChanged to recover after shell restarts or initial absence. Without
//! a registered watcher/host the window and hotkeys still work.
//!
//! Adapted from rec-rs src/app/menubar.rs (MIT OR Apache-2.0), NSStatusItem
//! replaced by ksni's StatusNotifierItem.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ksni::menu::{MenuItem, StandardItem};
use ksni::{Icon, ToolTip, Tray as KsniTray};

use super::Cmd;

/// What the tray shows, in one value (the app rebuilds it every update).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub recording: bool,
    pub paused: bool,
    pub finishing: bool,
    /// Recorded time, `mm:ss` (from `record::clock`), empty when idle.
    pub clock: String,
    /// There is a last recording to reveal.
    pub has_last: bool,
    /// The replay buffer is on.
    pub replay: bool,
    /// A stream is running.
    pub live: bool,
    /// Flatpak updates the app itself: no "Check for Updates…".
    pub updates: bool,
    /// The global hotkey was registered (the menu says so when it wasn't).
    pub hotkey_ok: bool,
}

/// One row of the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Action {
        label: String,
        enabled: bool,
        cmd: Cmd,
    },
    /// A non-clickable line (the hotkey hint).
    Label(String),
    Separator,
}

/// The menu, in upstream's order.
pub fn menu(v: &View) -> Vec<Entry> {
    let action = |label: &str, enabled: bool, cmd: Cmd| Entry::Action {
        label: label.to_string(),
        enabled,
        cmd,
    };
    let mut m = vec![
        action(
            if v.finishing {
                "Saving…"
            } else if v.recording {
                "Stop Recording"
            } else {
                "Start Recording"
            },
            !v.finishing,
            Cmd::ToggleRecording,
        ),
        action(
            if v.paused { "Resume" } else { "Pause" },
            v.recording,
            Cmd::TogglePause,
        ),
        action("Save Replay", v.replay, Cmd::SaveReplay),
        action(
            if v.live { "Stop Streaming" } else { "Go Live" },
            true,
            Cmd::ToggleStream,
        ),
        Entry::Separator,
        action("Show Rec Window", true, Cmd::ShowWindow),
        action("Settings…", true, Cmd::ShowSettings),
        Entry::Separator,
        action("Reveal Last Recording", v.has_last, Cmd::RevealLast),
        action("Open Recordings Folder", true, Cmd::OpenFolder),
    ];
    if v.updates {
        m.push(action("Check for Updates…", true, Cmd::CheckForUpdates));
    }
    m.push(Entry::Separator);
    m.push(Entry::Label(if v.hotkey_ok {
        "Global hotkey: Ctrl+Shift+R".into()
    } else {
        "Global hotkey unavailable".into()
    }));
    m.push(action("Quit Rec", true, Cmd::Quit));
    m
}

/// The status item's title: the elapsed time while recording (with a red dot
/// drawn in the icon), " LIVE" while streaming, " Saving…" while finalizing.
pub fn title(v: &View) -> String {
    if v.finishing {
        " Saving…".into()
    } else if v.recording {
        format!(" {}", v.clock)
    } else if v.live {
        " LIVE".into()
    } else {
        String::new()
    }
}

/// The tooltip: the same state in words, for hosts that show no title.
pub fn tooltip(v: &View) -> String {
    if v.finishing {
        "Rec — saving the recording".into()
    } else if v.paused {
        format!("Rec — paused at {}", v.clock)
    } else if v.recording {
        format!("Rec — recording {}", v.clock)
    } else if v.live {
        "Rec — live".into()
    } else {
        "Rec".into()
    }
}

/// Whether the icon is the "live" (red) variant.
pub fn icon_alert(v: &View) -> bool {
    v.recording || v.live
}

/// The icon: a 64×64 ARGB32 (network byte order) rounded square with a red dot
/// while recording or live, Rec's grey otherwise. Drawn here so the tray needs
/// no icon theme.
pub fn icon_argb(v: &View, size: u32) -> Vec<u8> {
    let n = size as f32;
    let c = n / 2.0;
    let radius = n * 0.46;
    let dot = n * 0.30;
    // Recording: an opaque dark disc with a red dot; idle: a grey dot only,
    // which reads as a template icon on light and dark panels.
    let (fill, dot_color) = if icon_alert(v) {
        ([0x2bu8, 0x2b, 0x2d, 0xff], [0xffu8, 0x3b, 0x30, 0xff])
    } else {
        ([0, 0, 0, 0], [0xc8u8, 0xc8, 0xcc, 0xff])
    };
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let in_square = (px - c).abs() <= radius && (py - c).abs() <= radius;
            let in_dot = ((px - c).powi(2) + (py - c).powi(2)).sqrt() <= dot;
            // ARGB, network byte order: A, R, G, B.
            let px = if in_dot {
                [dot_color[3], dot_color[0], dot_color[1], dot_color[2]]
            } else if in_square {
                [fill[3], fill[0], fill[1], fill[2]]
            } else {
                [0, 0, 0, 0]
            };
            data.extend_from_slice(&px);
        }
    }
    data
}

fn icon(v: &View) -> Vec<Icon> {
    [22u32, 48]
        .into_iter()
        .map(|s| Icon {
            width: s as i32,
            height: s as i32,
            data: icon_argb(v, s),
        })
        .collect()
}

/// The ksni side: holds the current view and forwards clicks to the app.
pub struct Tray {
    pub view: View,
    send: Arc<dyn Fn(Cmd) + Send + Sync>,
    online: Arc<AtomicBool>,
}

impl KsniTray for Tray {
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        self.online.store(false, Ordering::Release);
        eprintln!("rec: warning: tray watcher lost; closing the window will quit Rec");
        super::wake();
        // ksni 0.3.7 service::run calls watcher_online BEFORE registration,
        // and skips the host check on that path. Stop this service; poll_restart
        // creates a fresh one whose spawn confirms both registration and host.
        false
    }

    fn id(&self) -> String {
        "rec-rs".into()
    }

    fn title(&self) -> String {
        title(&self.view)
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        icon(&self.view)
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: icon(&self.view),
            title: "Rec".into(),
            description: tooltip(&self.view),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        (self.send)(Cmd::ShowWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        menu(&self.view)
            .into_iter()
            .map(|e| match e {
                Entry::Separator => MenuItem::Separator,
                Entry::Label(label) => MenuItem::Standard(StandardItem {
                    label,
                    enabled: false,
                    ..Default::default()
                }),
                Entry::Action {
                    label,
                    enabled,
                    cmd,
                } => {
                    let send = self.send.clone();
                    MenuItem::Standard(StandardItem {
                        label,
                        enabled,
                        activate: Box::new(move |_| send(cmd)),
                        ..Default::default()
                    })
                }
            })
            .collect()
    }
}

/// A successfully registered item, invalidated immediately on watcher loss.
pub struct Handle {
    inner: ksni::blocking::Handle<Tray>,
    online: Arc<AtomicBool>,
    generation: Option<(Arc<AtomicU64>, u64)>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.inner.shutdown();
    }
}

impl Handle {
    pub fn available(&self) -> bool {
        self.online.load(Ordering::Acquire)
            && !self.inner.is_closed()
            && self
                .generation
                .as_ref()
                .is_none_or(|(current, registered)| current.load(Ordering::Acquire) == *registered)
    }
}

/// Drain one registration attempt without blocking. Failure remains window-only
/// until the next owner-change trigger; pending is not a confirmed handle.
pub fn poll_start(
    pending: &mut Option<std::sync::mpsc::Receiver<Option<Handle>>>,
) -> Option<Handle> {
    use std::sync::mpsc::TryRecvError;
    match pending.as_ref()?.try_recv() {
        Ok(handle) => {
            *pending = None;
            handle
        }
        Err(TryRecvError::Disconnected) => {
            *pending = None;
            None
        }
        Err(TryRecvError::Empty) => None,
    }
}

/// One cancellable signal listener per app, including sessions that initially
/// have no watcher. Name ownership is only a retry trigger, never availability.
/// The generation also invalidates registrations whose reply races an owner change.
pub struct Watcher {
    generation: Arc<AtomicU64>,
    present: Arc<AtomicBool>,
    attempted: u64,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    pub fn new() -> Option<Self> {
        use futures_util::StreamExt;
        let generation = Arc::new(AtomicU64::new(0));
        let present = Arc::new(AtomicBool::new(false));
        let epoch = generation.clone();
        let owned = present.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("rec-tray-watcher".into())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                rt.block_on(async {
                    let listen = async {
                        let conn = zbus::Connection::session().await?;
                        let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
                        let name = "org.kde.StatusNotifierWatcher";
                        // Install the match before the snapshot: a change during
                        // the query is queued and invalidates that snapshot too.
                        let mut changes = dbus
                            .receive_name_owner_changed_with_args(&[(0, name)])
                            .await?;
                        owned.store(
                            dbus.name_has_owner(name.try_into().unwrap()).await?,
                            Ordering::Release,
                        );
                        epoch.fetch_add(1, Ordering::AcqRel);
                        super::wake();
                        while let Some(change) = changes.next().await {
                            let args = change.args()?;
                            owned.store(args.new_owner().is_some(), Ordering::Release);
                            epoch.fetch_add(1, Ordering::AcqRel);
                            super::wake();
                        }
                        Ok::<(), zbus::Error>(())
                    };
                    futures_util::pin_mut!(listen);
                    // Dropping the app cancels even connection setup or a quiet
                    // signal stream; join cannot leave a parked listener behind.
                    let _ = futures_util::future::select(listen, stopped).await;
                    owned.store(false, Ordering::Release);
                    epoch.fetch_add(1, Ordering::AcqRel);
                    super::wake();
                });
            })
            .ok()?;
        Some(Self {
            generation,
            present,
            attempted: 0,
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Drain async results on the UI thread. Registration is attempted only after
/// the initial snapshot or NameOwnerChanged, not by polling D-Bus or a timer.
pub fn poll_restart(
    handle: &mut Option<Handle>,
    pending: &mut Option<std::sync::mpsc::Receiver<Option<Handle>>>,
    watcher: &mut Watcher,
    view: &View,
    send: Arc<dyn Fn(Cmd) + Send + Sync>,
) {
    if let Some(registered) = poll_start(pending) {
        if registered.available() {
            if handle.is_some() {
                eprintln!("rec: tray watcher back; re-registered");
            }
            *handle = Some(registered);
        }
    }
    let generation = watcher.generation.load(Ordering::Acquire);
    if pending.is_some() || generation == watcher.attempted {
        return;
    }
    watcher.attempted = generation;
    if !watcher.present.load(Ordering::Acquire) {
        return;
    }
    let view = view.clone();
    let epoch = watcher.generation.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    // At most one registration in flight, even during repeated shell restarts.
    if let Err(e) = std::thread::Builder::new()
        .name("rec-tray-register".into())
        .spawn(move || {
            let registered = start(view, send).map(|mut handle| {
                handle.generation = Some((epoch, generation));
                handle
            });
            let _ = tx.send(registered);
            super::wake();
        })
    {
        eprintln!("rec: warning: cannot restart tray worker ({e})");
        return;
    }
    *pending = Some(rx);
}

/// Start the tray. `None` (with a warning) when the session has no
/// StatusNotifierWatcher, so the app keeps working without one.
pub fn start(view: View, send: Arc<dyn Fn(Cmd) + Send + Sync>) -> Option<Handle> {
    use ksni::blocking::TrayMethods;
    // Flatpak: owning the well-known name is not allowed inside the sandbox.
    let flatpak = super::install::is_flatpak();
    // Only returned after spawn confirms registration AND a registered host.
    // Setting this before spawn also preserves an offline callback racing it.
    let online = Arc::new(AtomicBool::new(true));
    let tray = Tray {
        view,
        send,
        online: online.clone(),
    };
    match tray.disable_dbus_name(flatpak).spawn() {
        Ok(inner) => Some(Handle {
            inner,
            online,
            generation: None,
        }),
        Err(e) => {
            eprintln!("rec: warning: no tray item ({e}); closing the window will quit Rec");
            None
        }
    }
}

/// Update the item's state (title, icon, menu).
pub fn update(handle: &Handle, view: View) {
    handle.inner.update(move |t| {
        if t.view != view {
            t.view = view;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn idle() -> View {
        View {
            updates: true,
            hotkey_ok: true,
            ..Default::default()
        }
    }

    fn labels(v: &View) -> Vec<String> {
        menu(v)
            .into_iter()
            .filter_map(|e| match e {
                Entry::Action { label, .. } | Entry::Label(label) => Some(label),
                Entry::Separator => None,
            })
            .collect()
    }

    #[test]
    fn pending_failed_and_disconnected_startup_are_not_available() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut pending = Some(rx);
        assert!(poll_start(&mut pending).is_none());
        assert!(pending.is_some());
        tx.send(None).unwrap();
        assert!(poll_start(&mut pending).is_none());
        assert!(pending.is_none());

        let (tx, rx) = std::sync::mpsc::channel();
        let mut pending = Some(rx);
        drop(tx); // A startup worker that panicked / could not be started.
        assert!(poll_start(&mut pending).is_none());
        assert!(pending.is_none());
    }

    /// Run under dbus-run-session: no watcher and no desktop/portal needed.
    #[test]
    #[ignore = "requires an isolated session bus without a StatusNotifierWatcher"]
    fn absent_watcher_does_not_create_a_handle() {
        assert!(start(idle(), Arc::new(|_| panic!("unexpected command"))).is_none());
    }

    #[test]
    fn watcher_loss_invalidates_the_item_and_stops_the_service() {
        let online = Arc::new(AtomicBool::new(true));
        let tray = Tray {
            view: idle(),
            send: Arc::new(|_| panic!("watcher loss must not issue recording commands")),
            online: online.clone(),
        };
        assert!(!tray.watcher_offline(ksni::OfflineReason::No));
        assert!(!online.load(Ordering::Acquire));
    }

    struct FakeWatcher {
        calls: std::sync::mpsc::Sender<String>,
        release: Arc<tokio::sync::Notify>,
        reject: Arc<AtomicBool>,
        host: Arc<AtomicBool>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl FakeWatcher {
        async fn register_status_notifier_item(&self, service: String) -> zbus::fdo::Result<()> {
            self.calls.send(service).unwrap();
            // A name owner alone must not make the tray available. Hold the
            // method reply until the test explicitly accepts/rejects it.
            self.release.notified().await;
            if self.reject.load(Ordering::Acquire) {
                Err(zbus::fdo::Error::Failed(
                    "test registration rejected".into(),
                ))
            } else {
                Ok(())
            }
        }

        #[zbus(property)]
        fn is_status_notifier_host_registered(&self) -> bool {
            self.host.load(Ordering::Acquire)
        }
    }

    /// Each invocation owns its daemon; no process-global environment mutation,
    /// shared desktop bus, sleeps as synchronization, or detached child daemon.
    #[test]
    fn private_bus_watcher_restart_confirms_registration_before_hiding() {
        const CHILD: &str = "REC_TRAY_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            use std::io::BufRead;
            use std::process::{Command, Stdio};
            struct Bus(std::process::Child);
            impl Drop for Bus {
                fn drop(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
            let mut bus = Bus(Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped())
                .spawn()
                .expect("start private dbus-daemon"));
            let mut address = String::new();
            std::io::BufReader::new(bus.0.stdout.take().unwrap())
                .read_line(&mut address)
                .unwrap();
            assert!(!address.trim().is_empty());
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "app::tray::tests::private_bus_watcher_restart_confirms_registration_before_hiding", "--nocapture"])
                .env(CHILD, "1")
                .env("DBUS_SESSION_BUS_ADDRESS", address.trim())
                .env_remove("FLATPAK_ID")
                .status().unwrap();
            assert!(status.success(), "private bus child failed: {status}");
            return;
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(20), async {
                    let (calls_tx, calls) = std::sync::mpsc::channel();
                    let release = Arc::new(tokio::sync::Notify::new());
                    let reject = Arc::new(AtomicBool::new(false));
                    let host = Arc::new(AtomicBool::new(true));
                    let make_watcher = || FakeWatcher {
                        calls: calls_tx.clone(),
                        release: release.clone(),
                        reject: reject.clone(),
                        host: host.clone(),
                    };
                    let watcher = zbus::connection::Builder::session()
                        .unwrap()
                        .serve_at("/StatusNotifierWatcher", make_watcher())
                        .unwrap()
                        .build()
                        .await
                        .unwrap();
                    let name = "org.kde.StatusNotifierWatcher";
                    let mut recovery = Watcher::new().unwrap();
                    // Wait for the subscribed initial snapshot with no owner.
                    while recovery.generation.load(Ordering::Acquire) == 0 {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert!(!recovery.present.load(Ordering::Acquire));
                    let epoch = recovery.generation.clone();
                    let mut poll_restart =
                        |handle: &mut Option<Handle>,
                         pending: &mut Option<std::sync::mpsc::Receiver<Option<Handle>>>,
                         view: &View,
                         send: Arc<dyn Fn(Cmd) + Send + Sync>| {
                            super::poll_restart(handle, pending, &mut recovery, view, send);
                        };
                    // Initial absence must not disable later recovery.
                    watcher.request_name(name).await.unwrap();
                    let (commands_tx, commands) = std::sync::mpsc::channel();
                    let send: Arc<dyn Fn(Cmd) + Send + Sync> = Arc::new(move |c| {
                        commands_tx.send(c).unwrap();
                    });
                    let mut pending = None;
                    let mut handle = None;
                    // First registration is also unavailable until its reply.
                    loop {
                        poll_restart(&mut handle, &mut pending, &idle(), send.clone());
                        if calls.try_recv().is_ok() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert!(handle.is_none());
                    // Change ownership while the successful reply is held.
                    // Even if ksni accepts it, that stale worker must never be
                    // published as available for the replacement owner.
                    let before = epoch.load(Ordering::Acquire);
                    watcher.release_name(name).await.unwrap();
                    watcher.request_name(name).await.unwrap();
                    while epoch.load(Ordering::Acquire) < before + 2 {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    release.notify_one();
                    loop {
                        poll_restart(&mut handle, &mut pending, &idle(), send.clone());
                        assert!(handle.is_none());
                        if calls.try_recv().is_ok() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    release.notify_one();
                    while handle.is_none() {
                        poll_restart(&mut handle, &mut pending, &idle(), send.clone());
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert!(handle.as_ref().unwrap().available());
                    // Close the connection entirely; the replacement below
                    // has a different unique owner, like a restarted shell.
                    watcher.close().await.unwrap();
                    while handle.as_ref().unwrap().available() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert_eq!(
                        super::super::hide_action(true, handle.as_ref().unwrap().available()),
                        super::super::HideAction::Quit
                    );
                    // No timer/probe should attempt registration while absent.
                    poll_restart(&mut handle, &mut pending, &idle(), send.clone());
                    while pending.is_some() {
                        poll_restart(&mut handle, &mut pending, &idle(), send.clone());
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert!(!handle.as_ref().unwrap().available());
                    assert!(calls.try_recv().is_err());
                    let watcher = zbus::connection::Builder::session()
                        .unwrap()
                        .serve_at("/StatusNotifierWatcher", make_watcher())
                        .unwrap()
                        .build()
                        .await
                        .unwrap();
                    watcher.request_name(name).await.unwrap();
                    // A returned watcher that rejects registration is not usable.
                    reject.store(true, Ordering::Release);
                    loop {
                        poll_restart(&mut handle, &mut pending, &idle(), send.clone());
                        if calls.try_recv().is_ok() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert!(!handle.as_ref().unwrap().available());
                    assert!(pending.is_some());
                    release.notify_one();
                    while pending.is_some() {
                        poll_restart(&mut handle, &mut pending, &idle(), send.clone());
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert!(!handle.as_ref().unwrap().available());
                    reject.store(false, Ordering::Release);
                    // Even a successful registration needs a registered host.
                    host.store(false, Ordering::Release);
                    watcher.release_name(name).await.unwrap();
                    watcher.request_name(name).await.unwrap();
                    loop {
                        poll_restart(&mut handle, &mut pending, &idle(), send.clone());
                        if calls.try_recv().is_ok() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    release.notify_one();
                    while pending.is_some() {
                        poll_restart(&mut handle, &mut pending, &idle(), send.clone());
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert!(!handle.as_ref().unwrap().available());
                    host.store(true, Ordering::Release);
                    watcher.release_name(name).await.unwrap();
                    watcher.request_name(name).await.unwrap();
                    let service = loop {
                        poll_restart(&mut handle, &mut pending, &idle(), send.clone());
                        if let Ok(service) = calls.try_recv() {
                            break service;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    };
                    assert!(!handle.as_ref().unwrap().available());
                    release.notify_one();
                    while !handle.as_ref().unwrap().available() {
                        poll_restart(&mut handle, &mut pending, &idle(), send.clone());
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert_eq!(
                        super::super::hide_action(true, handle.as_ref().unwrap().available()),
                        super::super::HideAction::DestroyWindow
                    );
                    assert_eq!(
                        super::super::hide_action(false, handle.as_ref().unwrap().available()),
                        super::super::HideAction::SetInvisible
                    );
                    // Confirm the newly registered item is callable, not merely
                    // a successful fake method reply or an old exported object.
                    let item = zbus::Proxy::new(
                        &watcher,
                        service.as_str(),
                        "/StatusNotifierItem",
                        "org.kde.StatusNotifierItem",
                    )
                    .await
                    .unwrap();
                    item.call::<_, _, ()>("Activate", &(0i32, 0i32))
                        .await
                        .unwrap();
                    assert_eq!(commands.try_recv().unwrap(), Cmd::ShowWindow);
                    assert!(commands.try_recv().is_err());
                    drop(handle);
                })
                .await
                .expect("tray lifecycle timed out");
            });
    }

    #[test]
    fn menu_matches_upstream_in_order() {
        let v = idle();
        assert_eq!(
            labels(&v),
            [
                "Start Recording",
                "Pause",
                "Save Replay",
                "Go Live",
                "Show Rec Window",
                "Settings…",
                "Reveal Last Recording",
                "Open Recordings Folder",
                "Check for Updates…",
                "Global hotkey: Ctrl+Shift+R",
                "Quit Rec",
            ]
        );
        // Separators: after Go Live, after Settings, before the hotkey line.
        let seps = menu(&v).iter().filter(|e| **e == Entry::Separator).count();
        assert_eq!(seps, 3);
        // Recording flips the first two rows and enables Pause.
        let v = View {
            recording: true,
            clock: "01:02".into(),
            ..idle()
        };
        let m = menu(&v);
        assert_eq!(
            m[0],
            Entry::Action {
                label: "Stop Recording".into(),
                enabled: true,
                cmd: Cmd::ToggleRecording
            }
        );
        assert_eq!(
            m[1],
            Entry::Action {
                label: "Pause".into(),
                enabled: true,
                cmd: Cmd::TogglePause
            }
        );
        // Paused: Resume. Finishing: Saving… and no second start.
        let v = View {
            recording: true,
            paused: true,
            ..idle()
        };
        assert_eq!(labels(&v)[1], "Resume");
        let v = View {
            finishing: true,
            ..idle()
        };
        assert_eq!(labels(&v)[0], "Saving…");
        assert!(!matches!(menu(&v)[0], Entry::Action { enabled: true, .. }));
        // Replay off: Save Replay is disabled; live: Stop Streaming.
        let v = View {
            live: true,
            ..idle()
        };
        assert_eq!(labels(&v)[3], "Stop Streaming");
        assert!(!matches!(
            menu(&idle())[2],
            Entry::Action { enabled: true, .. }
        ));
        // No last recording: nothing to reveal (index 8; 0-3 the actions,
        // 4 separator, 5/6 window/settings, 7 separator, 8 reveal).
        assert!(!matches!(
            menu(&idle())[8],
            Entry::Action { enabled: true, .. }
        ));
        // Flatpak: no update item, and the hotkey line says when it failed.
        let v = View {
            updates: false,
            hotkey_ok: false,
            ..Default::default()
        };
        assert!(!labels(&v).iter().any(|l| l.contains("Check for Updates")));
        assert!(labels(&v).contains(&"Global hotkey unavailable".to_string()));
    }

    #[test]
    fn titles_match_upstream() {
        assert_eq!(title(&idle()), "");
        assert_eq!(tooltip(&idle()), "Rec");
        let v = View {
            recording: true,
            clock: "02:03".into(),
            ..idle()
        };
        assert_eq!(title(&v), " 02:03");
        assert_eq!(tooltip(&v), "Rec — recording 02:03");
        let v = View {
            recording: true,
            paused: true,
            clock: "02:03".into(),
            ..idle()
        };
        assert_eq!(title(&v), " 02:03");
        assert_eq!(tooltip(&v), "Rec — paused at 02:03");
        let v = View {
            live: true,
            ..idle()
        };
        assert_eq!(title(&v), " LIVE");
        assert_eq!(tooltip(&v), "Rec — live");
        let v = View {
            finishing: true,
            ..idle()
        };
        assert_eq!(title(&v), " Saving…");
        assert_eq!(tooltip(&v), "Rec — saving the recording");
    }

    #[test]
    fn icon_shows_a_red_dot_while_capturing() {
        let idle_icon = icon_argb(&idle(), 8);
        assert_eq!(idle_icon.len(), 8 * 8 * 4, "ARGB, 4 bytes per pixel");
        // The centre is the dot; the corner is transparent.
        let centre = 4 * (4 * 8 + 4);
        let corner = 0;
        assert_eq!(&idle_icon[corner..corner + 4], &[0, 0, 0, 0]);
        let v = View {
            recording: true,
            ..idle()
        };
        let rec = icon_argb(&v, 8);
        assert_eq!(&rec[centre..centre + 4], &[0xff, 0xff, 0x3b, 0x30]);
        // Idle is grey, and the square behind both is opaque Rec grey.
        assert_ne!(&idle_icon[centre..centre + 4], &rec[centre..centre + 4]);
        let edge = 4 * (4 * 8 + 1);
        assert_eq!(&rec[edge..edge + 4], &[0xff, 0x2b, 0x2b, 0x2d]);
        // Live without a recording also shows the alert icon.
        let v = View {
            live: true,
            ..idle()
        };
        assert!(icon_alert(&v));
        assert!(!icon_alert(&idle()));
    }
}
