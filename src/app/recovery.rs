//! Retry scheduling only; the app's startup/cleanup guards own capture exclusivity.
use std::time::{Duration, Instant};

/// Only transport/source loss is retryable. In particular a dismissed picker
/// must never be reopened automatically, regardless of the selected scene.
pub(super) fn retryable_screen_error(error: &str) -> bool {
    let error = error.to_lowercase();
    if [
        "cancel",
        "denied",
        "not allowed",
        "not permitted",
        "permission",
        "request failed",
        "refused",
        "accessdenied",
        "notauthorized",
        "invalid",
        "not found",
    ]
    .iter()
    .any(|needle| error.contains(needle))
    {
        return false;
    }
    [
        "screen capture stopped",
        "screen capture",
        "screen-capture portal",
        "desktop portal",
        "pipewire remote",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

#[derive(Default)]
pub(super) struct Recovery {
    failures: usize,
    deadline: Option<Instant>,
}
impl Recovery {
    pub fn failed(&mut self, now: Instant) {
        let delay = [2, 5, 10, 30][self.failures.min(3)];
        self.failures = self.failures.saturating_add(1);
        self.deadline = Some(now + Duration::from_secs(delay));
    }
    pub fn active(&self) -> bool {
        self.deadline.is_some()
    }
    pub fn due(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }
    pub fn expedite(&mut self, now: Instant) {
        if self.active() {
            self.deadline = Some(now);
        }
    }
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backoff_caps_and_success_resets() {
        let mut r = Recovery::default();
        let now = Instant::now();
        for seconds in [2, 5, 10, 30, 30] {
            r.failed(now);
            assert!(!r.due(now + Duration::from_secs(seconds - 1)));
            assert!(r.due(now + Duration::from_secs(seconds)));
        }
        r.reset();
        assert!(!r.active());
        r.failed(now);
        assert!(r.due(now + Duration::from_secs(2)));
    }
    #[test]
    fn triggers_only_expedite_existing_recovery() {
        let mut r = Recovery::default();
        let now = Instant::now();
        r.expedite(now);
        assert!(!r.due(now));
        r.failed(now);
        r.expedite(now);
        assert!(r.due(now));
    }
}
