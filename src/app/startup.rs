//! Process-wide serialization of options preparation and session startup.
//! A UI timeout/cancellation is NOT cancellation of a portal request. Keep the
//! lease until the worker and any abandoned session cleanup have really ended.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

static BUSY: AtomicBool = AtomicBool::new(false);
struct Lease;
impl Drop for Lease {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::SeqCst);
    }
}

pub struct Ready<T: Send + 'static> {
    value: Option<T>,
    cleanup: fn(T),
    lease: Option<Lease>,
}
impl<T: Send + 'static> Ready<T> {
    pub fn accept(mut self) -> T {
        self.value.take().unwrap()
    }

    /// Retain this handle when cleanup finalizes a recording: process exit
    /// must wait for it, unlike idle-preview teardown.
    pub fn discard(mut self) -> std::thread::JoinHandle<()> {
        let value = self.value.take().unwrap();
        let cleanup = self.cleanup;
        let lease = self.lease.take();
        std::thread::spawn(move || {
            cleanup(value);
            drop(lease);
            super::wake();
        })
    }
}
impl<T: Send + 'static> Drop for Ready<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            let cleanup = self.cleanup;
            let lease = self.lease.take();
            std::thread::spawn(move || {
                cleanup(value);
                drop(lease);
            });
        }
    }
}

pub struct Job<T: Send + 'static> {
    rx: mpsc::Receiver<Ready<T>>,
    cancelled: Arc<AtomicBool>,
}
impl<T: Send + 'static> Job<T> {
    pub fn spawn(work: impl FnOnce() -> T + Send + 'static, cleanup: fn(T)) -> Option<Self> {
        BUSY.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        let lease = Lease;
        let (tx, rx) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        std::thread::spawn(move || {
            // Wake on unwind too, so the UI observes the terminal disconnect.
            struct Wake;
            impl Drop for Wake {
                fn drop(&mut self) {
                    super::wake();
                }
            }
            let _wake = Wake;
            // Drop the sender before waking on unwind as well as success.
            let tx = tx;
            let lease = lease;
            let value = work();
            let ready = Ready {
                value: Some(value),
                cleanup,
                lease: Some(lease),
            };
            if !flag.load(Ordering::SeqCst) {
                let _ = tx.send(ready);
            }
        });
        Some(Self { rx, cancelled })
    }
    pub fn poll(&self) -> Result<Option<Ready<T>>, &'static str> {
        match self.rx.try_recv() {
            Ok(ready) => Ok(Some(ready)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err("session startup worker disconnected"),
        }
    }
}
impl<T: Send + 'static> Drop for Job<T> {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_never_releases_pending_portal_lease() {
        let (release, pending) = mpsc::channel();
        let (cleaned, done) = mpsc::channel();
        let job = Job::spawn(
            move || {
                pending.recv().unwrap();
                cleaned
            },
            |tx| {
                tx.send(()).unwrap();
            },
        )
        .unwrap();
        // Advancing a UI deadline (including 120s) cannot release this lease.
        drop(job);
        assert!(Job::spawn(|| (), |_| {}).is_none());
        release.send(()).unwrap();
        done.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        wait_idle();

        // A completed but unpolled result still owns the transaction. Dropping
        // the receiver (quit/stale request) must clean it up off the caller.
        let (cleaned, done) = mpsc::channel();
        let job = Job::spawn(
            move || cleaned,
            |tx| {
                tx.send(()).unwrap();
            },
        )
        .unwrap();
        let ready = wait_ready(&job);
        assert!(Job::spawn(|| (), |_| {}).is_none());
        drop(job);
        drop(ready);
        done.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        wait_idle();

        // Teardown can itself block (PipeWire/GPU). A stale completion must
        // not permit another picker until that teardown really ends.
        let (release_cleanup, cleanup_wait) = mpsc::channel();
        let (cleanup_started, cleanup_started_rx) = mpsc::channel();
        let job = Job::spawn(
            move || (cleanup_wait, cleanup_started),
            |(wait, started)| {
                started.send(()).unwrap();
                wait.recv().unwrap();
            },
        )
        .unwrap();
        let ready = wait_ready(&job);
        drop(ready);
        cleanup_started_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(Job::spawn(|| (), |_| {}).is_none());
        release_cleanup.send(()).unwrap();
        wait_idle();

        // Quit must retain and wait on a recording finalizer, even when
        // startup completed before its result was adopted. Use a blocked
        // cleanup to prove completion, not channel disconnect, is the fence.
        for cancel_before_ready in [false, true] {
            let (release_start, wait_start) = mpsc::channel();
            let (release_cleanup, wait_cleanup) = mpsc::channel();
            let (entered, cleanup_entered) = mpsc::channel();
            let job = Job::spawn(
                move || {
                    wait_start.recv().unwrap();
                    (wait_cleanup, entered)
                },
                |(wait, entered)| {
                    entered.send(()).unwrap();
                    wait.recv().unwrap();
                },
            )
            .unwrap();
            // App cancellation retains Job until its terminal result, whether
            // Stop/Quit happens during startup or after completion.
            if cancel_before_ready {
                assert!(matches!(job.poll(), Ok(None)));
            }
            release_start.send(()).unwrap();
            let ready = wait_ready(&job);
            drop(job);
            let cleanup = ready.discard();
            cleanup_entered
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert!(
                !cleanup.is_finished(),
                "quit could exit before finalization"
            );
            assert!(BUSY.load(Ordering::SeqCst));
            release_cleanup.send(()).unwrap();
            cleanup.join().unwrap();
            assert!(!BUSY.load(Ordering::SeqCst));
        }

        // Adoption transfers ownership without running abandoned cleanup.
        let job = Job::spawn(|| 42, |_| panic!("accepted value cleaned up")).unwrap();
        let ready = wait_ready(&job);
        assert_eq!(ready.accept(), 42);
        assert!(!BUSY.load(Ordering::SeqCst));
        drop(job);

        // Empty is pending, but a panicked worker is terminal and releases
        // the lease. The UI can clear Starting and offer another attempt.
        let (release, pending) = mpsc::channel();
        let job = Job::spawn(
            move || {
                pending.recv().unwrap();
                panic!("startup panic regression");
            },
            |_: ()| panic!("panic produced no value to clean"),
        )
        .unwrap();
        assert!(matches!(job.poll(), Ok(None)));
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match job.poll() {
                Err(e) => {
                    assert_eq!(e, "session startup worker disconnected");
                    break;
                }
                Ok(None) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "panic left startup pending"
                    );
                    std::thread::yield_now();
                }
                Ok(Some(_)) => panic!("panicked worker returned a value"),
            }
        }
        drop(job);
        wait_idle();
        let job = Job::spawn(|| 7, |_| {}).expect("panic kept startup lease");
        assert_eq!(wait_ready(&job).accept(), 7);
    }

    fn wait_ready<T: Send + 'static>(job: &Job<T>) -> Ready<T> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(ready) = job.poll().unwrap() {
                return ready;
            }
            assert!(std::time::Instant::now() < deadline, "startup stuck");
            std::thread::yield_now();
        }
    }

    fn wait_idle() {
        for _ in 0..1000 {
            if !BUSY.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("abandoned result did not release lease after cleanup");
    }
}
