//! One Rec per session, and control from a second launch.
//!
//! Starting `rec-rs` (or `rec-rs gui`) while Rec is already running must show
//! the running app's window instead of starting a second one — with its own
//! tray item, hotkeys and capture. A second launch therefore says what it wants
//! on a Unix socket and exits; the first instance listens on that socket.
//!
//! The socket lives in `$XDG_RUNTIME_DIR` (else `/tmp/rec-rs-$UID`, mode 0700),
//! next to the pidfile `rec-rs stop|pause|resume|save-replay` use. Those keep
//! going through the pidfile and signals (`record.rs`), so a shell controls the
//! app exactly like a headless recording.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

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

/// The listener of the one running app, and where it listens.
pub struct Listener {
    path: PathBuf,
    _thread: std::thread::JoinHandle<()>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
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
    if send_at(path, cmd).is_ok() {
        return Claim::AlreadyRunning;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // A leftover socket from a crash keeps bind() failing; this launch owns the
    // name now.
    let _ = std::fs::remove_file(path);
    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "rec: warning: could not listen on {} ({e}); a second Rec may start twice",
                path.display()
            );
            return Claim::Unavailable(rx);
        }
    };
    let path = path.to_path_buf();
    let thread = {
        let path = path.clone();
        std::thread::Builder::new()
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
                        Some(c) => {
                            let _ = tx.send(c);
                            let _ = conn.write_all(b"ok\n");
                        }
                        None => {
                            let _ = conn.write_all(b"unknown\n");
                        }
                    }
                }
                let _ = path;
            })
            .ok()
    };
    match thread {
        Some(t) => Claim::Listening {
            listener: Listener { path, _thread: t },
            rx,
        },
        None => Claim::Unavailable(rx),
    }
}

/// [`send`] to an explicit path (tests).
pub fn send_at(path: &Path, cmd: Control) -> std::io::Result<()> {
    let mut s = UnixStream::connect(path)?;
    s.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
    s.write_all(format!("{}\n", cmd.name()).as_bytes())?;
    let mut reply = String::new();
    let mut r = BufReader::new(s);
    r.read_line(&mut reply)?;
    if reply.trim() == "ok" {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "the running Rec answered {reply:?}"
        )))
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
        send_at(&path, Control::Settings).unwrap();
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
        send_at(&path, Control::Quit).unwrap();
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
}
