//! logind delay inhibition. The UI acknowledges suspend only after all recording
//! finalizers complete. logind may still suspend at InhibitDelayMaxSec: this is
//! a bounded grace period, not a guarantee against forced suspend or power loss.
use futures_util::StreamExt;
use std::{
    cell::Cell,
    sync::{Arc, mpsc},
};

pub struct SleepWatcher {
    events: mpsc::Receiver<(bool, u64)>,
    release: tokio::sync::mpsc::UnboundedSender<u64>,
    epoch: Cell<u64>,
    task: tokio::task::AbortHandle,
}
impl Drop for SleepWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl SleepWatcher {
    pub fn start(wake: Arc<dyn Fn() + Send + Sync>) -> Option<Self> {
        let (tx, events) = mpsc::channel();
        let (release, releases) = tokio::sync::mpsc::unbounded_channel();
        let (abort_tx, abort_rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("rec-sleep".into())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                rt.block_on(async move {
                    let task = tokio::spawn(async move {
                        let result = async {
                            let conn = zbus::Connection::system().await?;
                            watch(&conn, tx, releases, wake, None).await
                        }
                        .await;
                        if let Err(e) = result {
                            eprintln!("rec: sleep protection unavailable: {e}");
                        }
                    });
                    let _ = abort_tx.send(task.abort_handle());
                    let _ = task.await;
                });
            })
            .ok()?;
        Some(Self {
            events,
            release,
            epoch: Cell::new(0),
            task: abort_rx.recv().ok()?,
        })
    }
    pub fn drain(&self) -> Vec<bool> {
        self.events
            .try_iter()
            .map(|(sleeping, epoch)| {
                self.epoch.set(epoch);
                sleeping
            })
            .collect()
    }
    pub fn finalized(&self) {
        let _ = self.release.send(self.epoch.get());
    }
}
async fn inhibit(proxy: &zbus::Proxy<'_>) -> zbus::Result<zbus::zvariant::OwnedFd> {
    proxy
        .call(
            "Inhibit",
            &("sleep", "Rec", "Finalize recording before suspend", "delay"),
        )
        .await
}
async fn acquire_inhibitor(proxy: &zbus::Proxy<'_>) -> Option<zbus::zvariant::OwnedFd> {
    match inhibit(proxy).await {
        Ok(fd) => Some(fd),
        Err(e) => {
            eprintln!("rec: sleep delay protection unavailable (still watching sleep events): {e}");
            None
        }
    }
}
async fn watch(
    conn: &zbus::Connection,
    tx: mpsc::Sender<(bool, u64)>,
    mut releases: tokio::sync::mpsc::UnboundedReceiver<u64>,
    wake: Arc<dyn Fn() + Send + Sync>,
    ready: Option<tokio::sync::oneshot::Sender<()>>,
) -> zbus::Result<()> {
    let proxy = zbus::Proxy::new(
        conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    // Subscribe before acquiring: no suspend edge may fall into the setup gap.
    let mut signals = proxy.receive_signal("PrepareForSleep").await?;
    let mut guard = acquire_inhibitor(&proxy).await;
    let mut epoch = 0;
    let mut sleeping = false;
    if let Some(ready) = ready {
        let _ = ready.send(());
    }
    loop {
        tokio::select! {
            msg = signals.next() => {
                let Some(msg) = msg else { break; };
                if let Ok((value,)) = msg.body().deserialize::<(bool,)>() {
                    sleeping = value;
                    epoch += 1;
                    // Deliver resume even if renewing the delay inhibitor fails or stalls.
                    if tx.send((sleeping, epoch)).is_err() { break; }
                    wake();
                    if !sleeping { guard = acquire_inhibitor(&proxy).await; }
                }
            }
            release = releases.recv() => {
                let Some(release) = release else { break; };
                // Never let a delayed UI acknowledgement close a resume's new FD.
                if sleeping && release == epoch { guard.take(); }
            }
        }
    }
    drop(guard);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::BufRead,
        os::fd::{AsRawFd, FromRawFd},
        process::{Command, Stdio},
    };
    struct Manager(
        mpsc::Sender<std::os::fd::OwnedFd>,
        Arc<std::sync::atomic::AtomicBool>,
        Arc<std::sync::atomic::AtomicUsize>,
    );
    #[zbus::interface(name = "org.freedesktop.login1.Manager")]
    impl Manager {
        fn inhibit(
            &self,
            what: &str,
            who: &str,
            why: &str,
            mode: &str,
        ) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
            if self.1.load(std::sync::atomic::Ordering::SeqCst) {
                self.2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Err(zbus::fdo::Error::AccessDenied(
                    "test inhibitor failure".into(),
                ));
            }
            assert_eq!((what, who, mode), ("sleep", "Rec", "delay"));
            assert!(!why.is_empty());
            let mut fds = [0; 2];
            assert_eq!(
                unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) },
                0
            );
            let read = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[0]) };
            let write = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[1]) };
            self.0.send(read).unwrap();
            Ok(write.into())
        }
    }
    fn closed(fd: &std::os::fd::OwnedFd) -> bool {
        let mut byte = 0u8;
        unsafe { libc::read(fd.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) == 0 }
    }
    #[test]
    fn private_bus_holds_delay_until_finalized_and_reacquires_on_resume() {
        exercise_sleep_watcher(false);
    }
    #[test]
    fn private_bus_inhibitor_failure_keeps_watching_and_recovers() {
        exercise_sleep_watcher(true);
    }
    fn exercise_sleep_watcher(initial_failure: bool) {
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
            .unwrap());
        let mut address = String::new();
        std::io::BufReader::new(bus.0.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let (_control, control_rx) = mpsc::channel();
        let mut app = super::super::App::new(false, false, None, true, control_rx, None);
        app.main_visible = false;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    let (fds, received) = mpsc::channel();
                    let fail = Arc::new(std::sync::atomic::AtomicBool::new(initial_failure));
                    let failures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    let service = zbus::connection::Builder::address(address.trim())
                        .unwrap()
                        .name("org.freedesktop.login1")
                        .unwrap()
                        .serve_at(
                            "/org/freedesktop/login1",
                            Manager(fds, fail.clone(), failures.clone()),
                        )
                        .unwrap()
                        .build()
                        .await
                        .unwrap();
                    let client = zbus::connection::Builder::address(address.trim())
                        .unwrap()
                        .build()
                        .await
                        .unwrap();
                    let (tx, events) = mpsc::channel();
                    let (release, releases) = tokio::sync::mpsc::unbounded_channel();
                    let (ready, wait) = tokio::sync::oneshot::channel();
                    let task = tokio::spawn(async move {
                        watch(&client, tx, releases, Arc::new(|| {}), Some(ready))
                            .await
                            .unwrap()
                    });
                    wait.await.unwrap();
                    app.sleep = Some(SleepWatcher {
                        events,
                        release,
                        epoch: Cell::new(0),
                        task: task.abort_handle(),
                    });
                    if initial_failure {
                        // Startup failure must retain the subscription. Resume while
                        // Inhibit still fails must reach the UI, too.
                        for failing in [true, false] {
                            fail.store(failing, std::sync::atomic::Ordering::SeqCst);
                            for sleeping in [true, false] {
                                service
                                    .emit_signal(
                                        None::<&str>,
                                        "/org/freedesktop/login1",
                                        "org.freedesktop.login1.Manager",
                                        "PrepareForSleep",
                                        &(sleeping,),
                                    )
                                    .await
                                    .unwrap();
                                loop {
                                    if app.sleep.as_ref().unwrap().drain() == [sleeping] {
                                        break;
                                    }
                                    assert!(
                                        !task.is_finished(),
                                        "watcher exited on inhibitor failure"
                                    );
                                    tokio::task::yield_now().await;
                                }
                                // Wait until renewal has actually failed before
                                // changing the fake service's failure mode.
                                if !sleeping && failing {
                                    while failures.load(std::sync::atomic::Ordering::SeqCst) < 2 {
                                        assert!(!task.is_finished());
                                        tokio::task::yield_now().await;
                                    }
                                    assert!(received.try_recv().is_err());
                                    assert!(!task.is_finished());
                                }
                            }
                        }
                    }
                    let mut fd = loop {
                        if let Ok(fd) = received.try_recv() {
                            break fd;
                        }
                        tokio::task::yield_now().await;
                    };
                    for recording in [false, true] {
                        assert!(!closed(&fd)); // acquired BEFORE PrepareForSleep
                        service
                            .emit_signal(
                                None::<&str>,
                                "/org/freedesktop/login1",
                                "org.freedesktop.login1.Manager",
                                "PrepareForSleep",
                                &(true,),
                            )
                            .await
                            .unwrap();
                        loop {
                            if app.sleep.as_ref().unwrap().drain() == [true] {
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                        app.prepare_for_sleep(true);
                        let epoch = app.sleep.as_ref().unwrap().epoch.get();
                        if recording {
                            // Exercise the application's actual release gate while a
                            // writer finalizes, then while startup cleanup is retained.
                            app.st.finishing = true;
                            app.release_sleep_delay_if_finalized();
                            for _ in 0..100 {
                                tokio::task::yield_now().await;
                                assert!(!closed(&fd));
                            }
                            app.st.finishing = false;
                            let (done, wait) = mpsc::channel();
                            app.recording_start_cleanup
                                .push(std::thread::spawn(move || {
                                    wait.recv().unwrap();
                                }));
                            app.release_sleep_delay_if_finalized();
                            for _ in 0..100 {
                                tokio::task::yield_now().await;
                                assert!(!closed(&fd));
                            }
                            done.send(()).unwrap();
                            app.recording_start_cleanup.pop().unwrap().join().unwrap();
                        }
                        app.release_sleep_delay_if_finalized();
                        while !closed(&fd) {
                            tokio::task::yield_now().await;
                        }
                        service
                            .emit_signal(
                                None::<&str>,
                                "/org/freedesktop/login1",
                                "org.freedesktop.login1.Manager",
                                "PrepareForSleep",
                                &(false,),
                            )
                            .await
                            .unwrap();
                        loop {
                            if app.sleep.as_ref().unwrap().drain() == [false] {
                                app.prepare_for_sleep(false);
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                        fd = loop {
                            if let Ok(fd) = received.try_recv() {
                                break fd;
                            }
                            tokio::task::yield_now().await;
                        };
                        app.sleep.as_ref().unwrap().release.send(epoch).unwrap(); // stale acknowledgement cannot close new inhibitor
                        for _ in 0..100 {
                            tokio::task::yield_now().await;
                        }
                        assert!(!closed(&fd));
                    }
                    task.abort();
                })
                .await
                .unwrap();
            });
    }
}
