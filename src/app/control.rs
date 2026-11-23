//! One Rec per session, and control from a second launch or the shell.
//!
//! Starting `rec-rs` (or `rec-rs gui`) while Rec is already running must show
//! the running app's window instead of starting a second one — with its own
//! tray item, hotkeys and capture. A second launch therefore says what it wants
//! on a Unix socket and exits; the first instance listens on that socket.
//!
//! `rec-rs stop|pause|resume|save-replay` use that socket too, and only fall
//! back to the pidfile and a signal when nothing listens. Inside a Flatpak each
//! instance has its own PID namespace (the pid in the pidfile is meaningless in
//! another instance, and its signal hits the wrong process), while
//! `$XDG_RUNTIME_DIR` is shared — the socket is what makes those commands work
//! from a second `flatpak run`. A CLI `rec-rs record` listens on the same
//! socket with the same protocol, so both kinds of recorder answer.
//!
//! The socket lives in `$XDG_RUNTIME_DIR` (else `/tmp/rec-rs-$UID`, mode 0700)
//! and is created 0600: one user's control channel. The pidfile stays for the
//! file path and for the signal fallback (`kill -USR1`, SIGTERM, SIGRTMIN+1).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crate::util::runtime_dir;

/// What a second launch asks the running app to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// Show the main window (a plain `rec-rs`).
    Show,
    /// Show the settings window (`--settings`).
    Settings,
    /// Start recording (`--start-recording`; no-op when already recording).
    StartRecording,
    /// Toggle recording (the tray/hotkey command, also usable from scripts).
    ToggleRecording,
    /// Stop a running recording and finalize the file.
    Stop,
    Pause,
    Resume,
    SaveReplay,
    /// Quit (finalizing a recording first).
    Quit,
}

impl Control {
    pub fn name(self) -> &'static str {
        match self {
            Control::Show => "show",
            Control::Settings => "settings",
            Control::StartRecording => "start-recording",
            Control::ToggleRecording => "toggle-recording",
            Control::Stop => "stop",
            Control::Pause => "pause",
            Control::Resume => "resume",
            Control::SaveReplay => "save-replay",
            Control::Quit => "quit",
        }
    }

    pub fn parse(text: &str) -> Option<Control> {
        Some(match text.trim() {
            "show" => Control::Show,
            "settings" => Control::Settings,
            "start-recording" => Control::StartRecording,
            "toggle-recording" => Control::ToggleRecording,
            "stop" => Control::Stop,
            "pause" => Control::Pause,
            "resume" => Control::Resume,
            "save-replay" => Control::SaveReplay,
            "quit" => Control::Quit,
            _ => return None,
        })
    }
}

/// The single-instance socket.
pub fn socket_path() -> PathBuf {
    runtime_dir().join("rec-rs-control.sock")
}

/// What the listener answers for a command: `true` handled, `false` not a
/// command this program implements (the caller's "unknown").
pub type Handler = Arc<dyn Fn(Control) -> bool + Send + Sync>;

/// A socket that answers control commands, and the thread serving it.
pub struct Server {
    path: PathBuf,
    _thread: std::thread::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Answer control commands on [`socket_path`].
pub fn serve(handler: Handler) -> std::io::Result<Server> {
    serve_with(&socket_path(), handler)
}

/// [`serve`] at an explicit path (tests).
///
/// A leftover socket from a crash is replaced (remove only after a connect
/// failed, so a live listener is never stolen); a *live* listener makes this
/// fail with `AddrInUse`, which is how a second recorder refuses to start.
pub fn serve_with(path: &Path, handler: Handler) -> std::io::Result<Server> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "another recorder is running",
                ));
            }
            // Stale: nothing answers, so the name is free.
            let _ = std::fs::remove_file(path);
            UnixListener::bind(path)?
        }
        Err(e) => return Err(e),
    };
    // One user's control channel: 0600 (the directory is already 0700 in the
    // /tmp fallback, and shared within a Flatpak sandbox otherwise).
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let path_owned = path.to_path_buf();
    let thread = std::thread::Builder::new()
        .name("rec-control".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let mut line = String::new();
                let read = {
                    let mut r = BufReader::new(&conn);
                    r.read_line(&mut line)
                };
                if read.is_err() {
                    continue;
                }
                match Control::parse(&line) {
                    Some(c) if (handler)(c) => {
                        let _ = conn.write_all(b"ok\n");
                    }
                    Some(_) | None => {
                        let _ = conn.write_all(b"unknown\n");
                    }
                }
            }
            let _ = path_owned;
        })?;
    Ok(Server {
        path: path.to_path_buf(),
        _thread: thread,
    })
}

/// The listener of the one running app, and where it listens.
pub struct Listener {
    #[expect(dead_code, reason = "dropping it stops the listener")]
    pub server: Server,
}

/// What this launch is: the one running app, or a second launch that already
/// told the first one what it wanted.
pub enum Claim {
    /// Another Rec is running and got `cmd`; this launch exits.
    AlreadyRunning,
    /// This launch listens for later launches.
    Listening {
        listener: Listener,
        rx: Receiver<Control>,
    },
    /// No other instance, and the socket could not be opened: run anyway, so a
    /// broken runtime dir never keeps the user from recording.
    Unavailable(Receiver<Control>),
}

/// Claim the instance: hand `cmd` to an already-running app, or become the
/// listener.
///
/// The stale-socket case (a crash left the file behind) is handled by removing
/// the socket only after a connect failed, so two launches racing each other
/// still end up with one instance.
pub fn claim(cmd: Control) -> Claim {
    claim_at(&socket_path(), cmd)
}

/// [`claim`] at an explicit path (tests).
pub fn claim_at(path: &Path, cmd: Control) -> Claim {
    let (tx, rx) = std::sync::mpsc::channel();
    // A running *app* answers and takes the command; a CLI recorder answers
    // "unknown" for app-only commands, and the app then runs without the socket
    // (the recorder already serves `stop`/`pause`/`resume`/`save-replay`).
    if matches!(send_at(path, cmd), Ok(true)) {
        return Claim::AlreadyRunning;
    }
    let handler: Handler = Arc::new(move |c| tx.send(c).is_ok());
    match serve_with(path, handler) {
        Ok(server) => Claim::Listening {
            listener: Listener { server },
            rx,
        },
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!(
                "rec: warning: a recorder already owns {}; this Rec runs without its window controls",
                path.display()
            );
            Claim::Unavailable(rx)
        }
        Err(e) => {
            eprintln!(
                "rec: warning: could not listen on {} ({e}); a second Rec may start twice",
                path.display()
            );
            Claim::Unavailable(rx)
        }
    }
}

/// Tell the running Rec (app or CLI recorder) to do `cmd`.
///
/// `Ok(true)` = it handled the command, `Ok(false)` = it does not implement it,
/// `Err` = nothing listens (or the socket is stale: see
/// [`remove_stale_socket`]).
pub fn send(cmd: Control) -> std::io::Result<bool> {
    send_at(&socket_path(), cmd)
}

/// [`send`] to an explicit path (tests).
pub fn send_at(path: &Path, cmd: Control) -> std::io::Result<bool> {
    let mut s = UnixStream::connect(path)?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(format!("{}\n", cmd.name()).as_bytes())?;
    let mut reply = String::new();
    let mut r = BufReader::new(s);
    r.read_line(&mut reply)?;
    match reply.trim() {
        "ok" => Ok(true),
        "unknown" => Ok(false),
        other => Err(std::io::Error::other(format!(
            "the running Rec answered {other:?}"
        ))),
    }
}

/// Remove a socket nothing listens on (a crash left it behind), so the next
/// [`send`] fails fast and the caller can use the pidfile signal instead.
pub fn remove_stale_socket() {
    let path = socket_path();
    if UnixStream::connect(&path).is_err() {
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
fn is_running_at(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rec-control-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("control.sock")
    }

    #[test]
    fn commands_round_trip_through_their_names() {
        for c in [
            Control::Show,
            Control::Settings,
            Control::StartRecording,
            Control::ToggleRecording,
            Control::Stop,
            Control::Pause,
            Control::Resume,
            Control::SaveReplay,
            Control::Quit,
        ] {
            assert_eq!(Control::parse(c.name()), Some(c), "{}", c.name());
        }
        assert_eq!(Control::parse(" show \n"), Some(Control::Show));
        assert_eq!(Control::parse("nope"), None);
        assert_eq!(Control::parse(""), None);
    }

    #[test]
    fn the_second_launch_talks_to_the_first() {
        let path = temp_path("second");
        let (listener, rx) = match claim_at(&path, Control::Show) {
            Claim::Listening { listener, rx } => (listener, rx),
            _ => panic!("the first launch listens"),
        };
        assert!(is_running_at(&path));
        // A second launch does not become a listener; it sends its command.
        assert!(matches!(send_at(&path, Control::Settings), Ok(true)));
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(2)),
            Ok(Control::Settings)
        );
        assert!(
            matches!(
                claim_at(&path, Control::StartRecording),
                Claim::AlreadyRunning
            ),
            "the second launch stands down"
        );
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(2)),
            Ok(Control::StartRecording)
        );
        // An unknown command is answered, not silently dropped.
        let mut s = UnixStream::connect(&path).unwrap();
        s.write_all(b"nonsense\n").unwrap();
        let mut reply = String::new();
        BufReader::new(s).read_line(&mut reply).unwrap();
        assert_eq!(reply.trim(), "unknown");
        drop(listener);
        assert!(!path.exists(), "the socket is cleaned up on exit");
    }

    #[test]
    fn a_stale_socket_is_replaced() {
        let path = temp_path("stale");
        // A file from a crash: nothing listens on it.
        std::fs::write(&path, b"not a socket").unwrap();
        assert!(!is_running_at(&path));
        let (listener, rx) = match claim_at(&path, Control::Show) {
            Claim::Listening { listener, rx } => (listener, rx),
            _ => panic!("the stale socket must not stop the app"),
        };
        assert!(is_running_at(&path));
        assert!(matches!(send_at(&path, Control::Quit), Ok(true)));
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(2)),
            Ok(Control::Quit)
        );
        drop(listener);
    }

    #[test]
    fn sending_where_nothing_listens_is_an_error() {
        let path = temp_path("nobody");
        assert!(send_at(&path, Control::Show).is_err());
        assert!(!is_running_at(&path));
    }

    /// A recorder that only implements the shell commands (the CLI recorder):
    /// it answers "unknown" for the app-only ones, and `send` says so instead of
    /// pretending the command ran.
    #[test]
    fn a_recorder_answers_only_the_commands_it_handles() {
        let path = temp_path("recorder");
        let seen = Arc::new(std::sync::Mutex::new(vec![]));
        let handler: Handler = {
            let seen = seen.clone();
            Arc::new(move |c| {
                let shell = matches!(
                    c,
                    Control::Stop | Control::Pause | Control::Resume | Control::SaveReplay
                );
                if shell {
                    seen.lock().unwrap().push(c);
                }
                shell
            })
        };
        let server = serve_with(&path, handler).expect("listens");
        assert!(matches!(send_at(&path, Control::Pause), Ok(true)));
        assert!(matches!(send_at(&path, Control::SaveReplay), Ok(true)));
        assert!(
            matches!(send_at(&path, Control::Show), Ok(false)),
            "an app-only command is not handled"
        );
        assert_eq!(*seen.lock().unwrap(), [Control::Pause, Control::SaveReplay]);
        // A second recorder refuses while the first is live (AddrInUse).
        let again = match serve_with(&path, Arc::new(|_| true)) {
            Err(e) => e,
            Ok(_) => panic!("a second recorder must refuse while the first listens"),
        };
        assert_eq!(again.kind(), std::io::ErrorKind::AddrInUse);
        assert_eq!(again.to_string(), "another recorder is running");
        drop(server);
        assert!(!path.exists());
        // Now the name is free again: a new recorder starts and serves.
        let server = serve_with(&path, Arc::new(|c| matches!(c, Control::Stop))).unwrap();
        assert!(matches!(send_at(&path, Control::Stop), Ok(true)));
        drop(server);
    }

    /// The socket is the user's own control channel: created 0600.
    #[test]
    fn the_socket_is_private() {
        let path = temp_path("mode");
        let server = serve_with(&path, Arc::new(|_| true)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket mode {mode:o}");
        drop(server);
    }
}
