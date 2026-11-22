//! Global hotkeys: Ctrl+Shift+R starts/stops a recording, Ctrl+Shift+S saves
//! the replay buffer while it is on (`⇧⌘R`/`⇧⌘S` with ⌘ mapped to Ctrl).
//!
//! On Wayland the XDG GlobalShortcuts portal owns the shortcuts: the first
//! registration shows the desktop's "what should this key do?" dialog, and the
//! desktop remembers the answer. On X11 (or a session where the portal is
//! missing) `XGrabKey` on the root window does the same job. When neither is
//! available, or another application already holds the key, registration fails
//! and the app says so in the window and in the tray — upstream's behaviour
//! when Carbon refused the shortcut.
//!
//! The default keys, the portal trigger strings and the X11 keycode/keysym
//! maths are pure functions with tests; the two backends run on their own
//! threads (the portal's dialog must never block the UI).
//!
//! Adapted from rec-rs src/hotkey.rs (MIT OR Apache-2.0), Carbon
//! `RegisterEventHotKey` replaced by the GlobalShortcuts portal and XGrabKey.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, mpsc};

/// The two shortcuts the app registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Shortcut {
    /// Start/stop recording.
    Record,
    /// Save the replay buffer (registered only while the buffer is on, as
    /// upstream: ⇧⌘S is "Save As" in most other apps).
    Replay,
}

impl Shortcut {
    pub fn id(self) -> &'static str {
        match self {
            Shortcut::Record => "record",
            Shortcut::Replay => "replay",
        }
    }

    /// What the desktop's shortcut dialog says.
    pub fn description(self) -> &'static str {
        match self {
            Shortcut::Record => "Start or stop a recording",
            Shortcut::Replay => "Save the replay buffer",
        }
    }

    /// The key's name in the desktop's UI.
    pub fn label(self) -> &'static str {
        match self {
            Shortcut::Record => "Ctrl+Shift+R",
            Shortcut::Replay => "Ctrl+Shift+S",
        }
    }

    /// The trigger string of the XDG shortcuts specification
    /// (`<modifiers>:<keysym>`): modifiers uppercase, key by keysym name.
    pub fn trigger(self) -> &'static str {
        match self {
            Shortcut::Record => "CTRL+SHIFT+r",
            Shortcut::Replay => "CTRL+SHIFT+s",
        }
    }

    /// The X11 keysym (`XK_r` / `XK_s`).
    pub fn keysym(self) -> u32 {
        match self {
            Shortcut::Record => 0x0072,
            Shortcut::Replay => 0x0073,
        }
    }
}

/// What happened to a registration, for the UI. The portal answers late (its
/// dialog runs asynchronously), so this is polled instead of returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Registered(Shortcut),
    Unavailable(Shortcut, String),
}

static EVENTS: Mutex<Vec<Event>> = Mutex::new(Vec::new());

/// Drain the registration results that arrived since the last call.
pub fn poll() -> Vec<Event> {
    std::mem::take(&mut *EVENTS.lock().unwrap())
}

fn post(e: Event) {
    EVENTS.lock().unwrap().push(e);
}

/// Which backend to use, from the session type: the portal on Wayland, X11
/// grabs when there is an X display to grab on, and nothing without either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Portal,
    X11,
    None,
}

pub fn choose_backend(wayland: bool, display: bool) -> Backend {
    if wayland {
        Backend::Portal
    } else if display {
        Backend::X11
    } else {
        Backend::None
    }
}

/// The session's backend, from the environment (`REC_HOTKEY_BACKEND=x11|portal`
/// overrides it for tests).
pub fn backend() -> Backend {
    if let Ok(v) = std::env::var("REC_HOTKEY_BACKEND") {
        return match v.as_str() {
            "none" => Backend::None,
            "portal" => Backend::Portal,
            _ => Backend::X11,
        };
    }
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let display = std::env::var_os("DISPLAY").is_some();
    choose_backend(wayland, display)
}

type Callback = Arc<dyn Fn() + Send + Sync>;

fn callbacks() -> &'static Mutex<HashMap<Shortcut, Callback>> {
    static CB: OnceLock<Mutex<HashMap<Shortcut, Callback>>> = OnceLock::new();
    CB.get_or_init(Default::default)
}

enum Msg {
    Bind(Shortcut),
    Unbind(Shortcut),
}

struct BackendHandle {
    kind: Backend,
    /// X11 and portal messages go through their own threads.
    tx: Option<mpsc::Sender<Msg>>,
    /// The portal channel is async (its dialog is awaited).
    portal_tx: Option<tokio::sync::mpsc::UnboundedSender<Msg>>,
}

fn handle() -> &'static Mutex<Option<BackendHandle>> {
    static H: OnceLock<Mutex<Option<BackendHandle>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(None))
}

/// Register `which` to call `cb` from anywhere in the session.
///
/// `Err` means no shortcut can be registered at all (no Wayland or X11
/// session, or the X11 grab was refused). With the portal the answer arrives
/// later, as an [`Event`] from [`poll`].
pub fn register(which: Shortcut, cb: impl Fn() + Send + Sync + 'static) -> Result<(), String> {
    callbacks().lock().unwrap().insert(which, Arc::new(cb));
    let mut h = handle().lock().unwrap();
    if h.is_none() {
        *h = Some(match backend() {
            Backend::X11 => {
                let tx = x11::start()?;
                BackendHandle {
                    kind: Backend::X11,
                    tx: Some(tx),
                    portal_tx: None,
                }
            }
            Backend::Portal => {
                let tx = portal::start()?;
                BackendHandle {
                    kind: Backend::Portal,
                    tx: None,
                    portal_tx: Some(tx),
                }
            }
            Backend::None => {
                return Err("no Wayland or X11 session (set DISPLAY or WAYLAND_DISPLAY)".into());
            }
        });
    }
    let h = h.as_ref().unwrap();
    match h.kind {
        Backend::X11 => {
            h.tx.as_ref()
                .ok_or("the X11 hotkey thread is gone")?
                .send(Msg::Bind(which))
                .map_err(|_| "the X11 hotkey thread is gone".to_string())
        }
        Backend::Portal => h
            .portal_tx
            .as_ref()
            .ok_or("the portal hotkey thread is gone")?
            .send(Msg::Bind(which))
            .map_err(|_| "the portal hotkey thread is gone".to_string()),
        Backend::None => Err("no shortcut backend".into()),
    }
}

/// Release `which` (no-op when it wasn't registered).
pub fn unregister(which: Shortcut) {
    callbacks().lock().unwrap().remove(&which);
    if let Some(h) = handle().lock().unwrap().as_ref() {
        match h.kind {
            Backend::X11 => {
                if let Some(tx) = &h.tx {
                    let _ = tx.send(Msg::Unbind(which));
                }
            }
            Backend::Portal => {
                if let Some(tx) = &h.portal_tx {
                    let _ = tx.send(Msg::Unbind(which));
                }
            }
            Backend::None => {}
        }
    }
}

fn fire(which: Shortcut) {
    let cb = callbacks().lock().unwrap().get(&which).cloned();
    if let Some(cb) = cb {
        cb();
    }
}

/// The X11 keycode for `keysym`, from a keyboard mapping.
pub fn keycode_for(
    keysyms: &[u32],
    min_keycode: u8,
    keysyms_per_keycode: u8,
    keysym: u32,
) -> Option<u8> {
    let per = keysyms_per_keycode as usize;
    if per == 0 {
        return None;
    }
    keysyms
        .chunks(per)
        .position(|set| set.contains(&keysym))
        .map(|i| min_keycode as usize + i)
        .filter(|k| *k <= u8::MAX as usize)
        .map(|k| k as u8)
}

/// The modifier masks to grab: the shortcut itself, plus the NumLock and
/// CapsLock variants (otherwise the shortcut dies while either lock is on).
pub fn x11_modifier_masks() -> [u16; 4] {
    use x11rb::protocol::xproto::ModMask;
    let base = u16::from(ModMask::CONTROL) | u16::from(ModMask::SHIFT);
    [
        base,
        base | u16::from(ModMask::LOCK),
        base | 0x10, // Mod2 is NumLock on every common layout.
        base | u16::from(ModMask::LOCK) | 0x10,
    ]
}

mod x11 {
    use super::*;
    use x11rb::connection::Connection;
    use x11rb::protocol::Event as XEvent;
    use x11rb::protocol::xproto::{ConnectionExt, GrabMode, ModMask};

    /// Connect and listen for key presses in the background. Grabs happen on
    /// [`Msg::Bind`], on the same thread.
    pub fn start() -> Result<mpsc::Sender<Msg>, String> {
        let (conn, screen) = x11rb::connect(None).map_err(|e| format!("X11: {e}"))?;
        let root = conn.setup().roots[screen].root;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("rec-hotkey-x11".into())
            .spawn(move || {
                let mut grabbed: HashMap<Shortcut, u8> = HashMap::new();
                loop {
                    // Grabs and ungrabs first, then one pass over the events
                    // (polling, so a command is never held up by an idle
                    // session) and a short sleep so a press is not busy-waited.
                    while let Ok(msg) = rx.try_recv() {
                        match msg {
                            Msg::Bind(which) => match grab(&conn, root, which) {
                                Ok(keycode) => {
                                    grabbed.insert(which, keycode);
                                    post(Event::Registered(which));
                                }
                                Err(e) => post(Event::Unavailable(which, e)),
                            },
                            Msg::Unbind(which) => {
                                if let Some(keycode) = grabbed.remove(&which) {
                                    for mask in x11_modifier_masks() {
                                        let _ = conn
                                            .ungrab_key(keycode, root, ModMask::from(mask))
                                            .map(|c| c.check());
                                    }
                                }
                            }
                        }
                    }
                    if let Some(keycode) = key_pressed(&conn)
                        && let Some(which) = grabbed
                            .iter()
                            .find(|(_, k)| **k == keycode)
                            .map(|(s, _)| *s)
                    {
                        fire(which);
                    }
                }
            })
            .map_err(|e| format!("could not start the hotkey thread: {e}"))?;
        Ok(tx)
    }

    /// Drain the event queue; the keycode of a grabbed key press, if any.
    fn key_pressed(conn: &impl Connection) -> Option<u8> {
        let mut pressed = None;
        while let Some(ev) = conn.poll_for_event().ok().flatten() {
            if let XEvent::KeyPress(k) = ev {
                pressed = Some(k.detail);
            }
        }
        if pressed.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        pressed
    }

    fn grab(conn: &impl Connection, root: u32, which: Shortcut) -> Result<u8, String> {
        let setup = conn.setup();
        let min = setup.min_keycode;
        let max = setup.max_keycode;
        let count = max.saturating_sub(min) + 1;
        let map = conn
            .get_keyboard_mapping(min, count)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let keycode = keycode_for(&map.keysyms, min, map.keysyms_per_keycode, which.keysym())
            .ok_or_else(|| format!("no keycode for {}", which.label()))?;
        for mask in x11_modifier_masks() {
            let r = conn
                .grab_key(
                    false,
                    root,
                    ModMask::from(mask),
                    keycode,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )
                .map_err(|e| e.to_string())?
                .check();
            if let Err(e) = r {
                // BadAccess: another client already owns this combination.
                return Err(format!(
                    "{} is already taken by another application ({e})",
                    which.label()
                ));
            }
        }
        Ok(keycode)
    }
}

mod portal {
    use super::*;
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
    use futures_util::StreamExt;

    type Session = ashpd::desktop::Session<GlobalShortcuts>;

    pub fn start() -> Result<tokio::sync::mpsc::UnboundedSender<Msg>, String> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("rec-hotkey-portal".into())
            .spawn(move || run(rx))
            .map_err(|e| format!("could not start the hotkey thread: {e}"))?;
        Ok(tx)
    }

    fn run(mut rx: tokio::sync::mpsc::UnboundedReceiver<Msg>) {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("rec: warning: no global hotkeys (no async runtime: {e})");
                return;
            }
        };
        rt.block_on(async move {
            let (gs, reason) = match GlobalShortcuts::new().await {
                Ok(gs) => (Some(gs), String::new()),
                Err(e) => (
                    None,
                    format!("the GlobalShortcuts portal is unavailable: {e}"),
                ),
            };
            let mut held: HashMap<Shortcut, (Session, tokio::task::JoinHandle<()>)> =
                HashMap::new();
            while let Some(msg) = rx.recv().await {
                match (gs.as_ref(), msg) {
                    (Some(gs), Msg::Bind(which)) => {
                        if let Some((session, task)) = held.remove(&which) {
                            task.abort();
                            let _ = session.close().await;
                        }
                        match bind(gs, which).await {
                            Ok((session, task)) => {
                                held.insert(which, (session, task));
                                post(Event::Registered(which));
                            }
                            Err(e) => post(Event::Unavailable(which, e)),
                        }
                    }
                    (Some(_), Msg::Unbind(which)) => {
                        if let Some((session, task)) = held.remove(&which) {
                            task.abort();
                            let _ = session.close().await;
                        }
                    }
                    // No portal: answer every request, so the app says so.
                    (None, Msg::Bind(which)) => {
                        post(Event::Unavailable(which, reason.clone()));
                    }
                    (None, Msg::Unbind(_)) => {}
                }
            }
        });
    }

    async fn bind(
        gs: &GlobalShortcuts,
        which: Shortcut,
    ) -> Result<(Session, tokio::task::JoinHandle<()>), String> {
        let session = gs
            .create_session(Default::default())
            .await
            .map_err(|e| format!("the portal refused a shortcut session: {e}"))?;
        let shortcut = NewShortcut::new(which.id(), which.description())
            .preferred_trigger(Some(which.trigger()));
        let request = gs
            .bind_shortcuts(&session, &[shortcut], None, Default::default())
            .await
            .map_err(|e| format!("the portal refused {}: {e}", which.label()))?;
        let bound = request
            .response()
            .map_err(|e| format!("{} was not registered: {e}", which.label()))?;
        // Some portals answer with an empty list when the user declined.
        if bound.shortcuts().is_empty() {
            return Err(format!(
                "{} is not registered with the desktop",
                which.label()
            ));
        }
        let mut activated = gs
            .receive_activated()
            .await
            .map_err(|e| format!("could not listen for {}: {e}", which.label()))?;
        let id = which.id().to_string();
        let task = tokio::spawn(async move {
            // One session per shortcut: an activation with our id is ours.
            while let Some(a) = activated.next().await {
                if a.shortcut_id() == id {
                    fire(which);
                }
            }
        });
        Ok((session, task))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_upstreams_keys() {
        assert_eq!(Shortcut::Record.label(), "Ctrl+Shift+R");
        assert_eq!(Shortcut::Replay.label(), "Ctrl+Shift+S");
        // The portal's trigger syntax: uppercase modifiers, keysym name.
        assert_eq!(Shortcut::Record.trigger(), "CTRL+SHIFT+r");
        assert_eq!(Shortcut::Replay.trigger(), "CTRL+SHIFT+s");
        assert_eq!(Shortcut::Record.keysym(), u32::from(b'r'));
        assert_eq!(Shortcut::Replay.keysym(), u32::from(b's'));
        assert_ne!(Shortcut::Record.id(), Shortcut::Replay.id());
        assert!(Shortcut::Record.description().contains("recording"));
    }

    #[test]
    fn backend_follows_the_session() {
        // Wayland: the portal, even with XWayland around (grabs do not work
        // across compositor windows).
        assert_eq!(choose_backend(true, true), Backend::Portal);
        assert_eq!(choose_backend(true, false), Backend::Portal);
        // X11: grabs.
        assert_eq!(choose_backend(false, true), Backend::X11);
        // A bare session (tests, a TTY): nothing, and the UI says so.
        assert_eq!(choose_backend(false, false), Backend::None);
    }

    #[test]
    fn x11_keycodes_come_from_the_mapping() {
        // Three keys per keycode, starting at 8: 'a','A','ae' / 'r','R','reg' /
        // 's','S','ssharp'.
        let keysyms = [
            0x61, 0x41, 0xE6, // a
            0x72, 0x52, 0xAE, // r
            0x73, 0x53, 0xDF, // s
        ];
        assert_eq!(keycode_for(&keysyms, 8, 3, 0x72), Some(9));
        assert_eq!(keycode_for(&keysyms, 8, 3, 0x73), Some(10));
        assert_eq!(keycode_for(&keysyms, 8, 3, 0x61), Some(8));
        assert_eq!(keycode_for(&keysyms, 8, 3, 0x99), None);
        assert_eq!(keycode_for(&[], 8, 0, 0x72), None);
    }

    #[test]
    fn x11_grabs_the_lock_key_variants() {
        let masks = x11_modifier_masks();
        // Control+Shift is always grabbed, and so are the lock variants (or
        // the shortcut dies while CapsLock or NumLock is on).
        assert!(masks.iter().all(|m| m & 0x4 != 0 && m & 0x1 != 0));
        assert!(masks.contains(&(0x4 | 0x1 | 0x2)));
        assert!(masks.contains(&(0x4 | 0x1 | 0x10)));
        assert_eq!(masks.len(), 4);
    }

    #[test]
    fn registration_results_are_queued_once() {
        let _ = poll();
        post(Event::Unavailable(Shortcut::Replay, "taken".into()));
        assert_eq!(
            poll(),
            [Event::Unavailable(Shortcut::Replay, "taken".into())]
        );
        assert!(poll().is_empty(), "events are drained");
        post(Event::Registered(Shortcut::Record));
        assert_eq!(poll(), [Event::Registered(Shortcut::Record)]);
    }

    #[test]
    fn a_hotkey_fires_its_callback() {
        let fired = Arc::new(Mutex::new(0));
        let f = fired.clone();
        callbacks()
            .lock()
            .unwrap()
            .insert(Shortcut::Record, Arc::new(move || *f.lock().unwrap() += 1));
        fire(Shortcut::Record);
        fire(Shortcut::Replay);
        assert_eq!(*fired.lock().unwrap(), 1, "only the registered one fires");
        callbacks().lock().unwrap().clear();
    }
}
