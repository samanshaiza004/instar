use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

pub use youth_state::{AppId, StateLocation};

use crate::RuntimeLimits;

pub trait NotificationDispatcher: Send + Sync {
    fn dispatch(&self, title: &str, body: &str);
}

/// Host clipboard access injected into the runtime.
///
/// A7 will provide native OS integration. Gate A4 defines the shared seam so
/// headless runtimes and tests can supply deterministic clipboard contents.
pub trait ClipboardService: Send + Sync {
    fn read_text(&self) -> Result<Option<String>, ClipboardError>;
    fn write_text(&self, text: &str) -> Result<(), ClipboardError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ClipboardError {
    #[error("clipboard service is unavailable")]
    Unavailable,
}

/// Native OS clipboard access via `arboard`.
///
/// Clipboard handles hold platform resources that are not always available
/// (e.g. a headless CI runner with no display server), so construction
/// never fails outright: when the platform clipboard cannot be opened,
/// this behaves as an always-empty clipboard (reads return `None`, writes
/// return [`ClipboardError::Unavailable`]) rather than panicking or
/// preventing the runtime from starting.
pub struct SystemClipboardService {
    clipboard: std::sync::Mutex<Option<arboard::Clipboard>>,
}

impl SystemClipboardService {
    #[must_use]
    pub fn new() -> Self {
        Self {
            clipboard: std::sync::Mutex::new(arboard::Clipboard::new().ok()),
        }
    }
}

impl Default for SystemClipboardService {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SystemClipboardService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemClipboardService")
            .finish_non_exhaustive()
    }
}

impl ClipboardService for SystemClipboardService {
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        let mut guard = self
            .clipboard
            .lock()
            .expect("system clipboard mutex is not poisoned");
        match guard.as_mut() {
            // A non-text clipboard (e.g. an image) or a genuinely empty
            // clipboard are the same "nothing to paste" case from an
            // Editor's point of view.
            Some(clipboard) => Ok(clipboard.get_text().ok()),
            None => Ok(None),
        }
    }

    fn write_text(&self, text: &str) -> Result<(), ClipboardError> {
        let mut guard = self
            .clipboard
            .lock()
            .expect("system clipboard mutex is not poisoned");
        match guard.as_mut() {
            Some(clipboard) => clipboard
                .set_text(text.to_owned())
                .map_err(|_| ClipboardError::Unavailable),
            None => Err(ClipboardError::Unavailable),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RecordingClipboardService {
    text: Arc<std::sync::Mutex<Option<String>>>,
}

impl RecordingClipboardService {
    #[must_use]
    pub fn text(&self) -> Option<String> {
        self.text
            .lock()
            .expect("recording clipboard-service mutex is not poisoned")
            .clone()
    }
}

impl ClipboardService for RecordingClipboardService {
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        Ok(self.text())
    }

    fn write_text(&self, text: &str) -> Result<(), ClipboardError> {
        *self
            .text
            .lock()
            .expect("recording clipboard-service mutex is not poisoned") = Some(text.to_owned());
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemNotificationDispatcher;

impl NotificationDispatcher for SystemNotificationDispatcher {
    fn dispatch(&self, title: &str, body: &str) {
        let result = std::panic::catch_unwind(|| {
            notify_rust::Notification::new()
                .summary(title)
                .body(body)
                .show()
        });
        match result {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(%error, "OS notification dispatch failed"),
            Err(_) => tracing::warn!("OS notification dispatch panicked"),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RecordingNotificationDispatcher {
    dispatched: Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

impl RecordingNotificationDispatcher {
    #[must_use]
    pub fn dispatched(&self) -> Vec<(String, String)> {
        self.dispatched
            .lock()
            .expect("recording notification-dispatcher mutex is not poisoned")
            .clone()
    }
}

impl NotificationDispatcher for RecordingNotificationDispatcher {
    fn dispatch(&self, title: &str, body: &str) {
        self.dispatched
            .lock()
            .expect("recording notification-dispatcher mutex is not poisoned")
            .push((title.to_owned(), body.to_owned()));
    }
}

pub trait GuestMonotonicClock: Send + Sync {
    fn resolution_nanoseconds(&self) -> u64;
    fn now_nanoseconds(&self) -> u64;
}

#[derive(Debug)]
pub struct SystemGuestMonotonicClock {
    started: Instant,
}

impl Default for SystemGuestMonotonicClock {
    fn default() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl GuestMonotonicClock for SystemGuestMonotonicClock {
    fn resolution_nanoseconds(&self) -> u64 {
        1
    }

    fn now_nanoseconds(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

#[derive(Clone, Debug, Default)]
pub struct VirtualGuestMonotonicClock {
    now_nanoseconds: Arc<std::sync::Mutex<u64>>,
    step_nanoseconds: u64,
}

impl VirtualGuestMonotonicClock {
    #[must_use]
    pub fn new(now_nanoseconds: u64) -> Self {
        Self {
            now_nanoseconds: Arc::new(std::sync::Mutex::new(now_nanoseconds)),
            step_nanoseconds: 0,
        }
    }

    #[must_use]
    pub fn with_step(mut self, step_nanoseconds: u64) -> Self {
        self.step_nanoseconds = step_nanoseconds;
        self
    }

    pub fn set(&self, now_nanoseconds: u64) {
        *self
            .now_nanoseconds
            .lock()
            .expect("virtual guest-clock mutex is not poisoned") = now_nanoseconds;
    }
}

impl GuestMonotonicClock for VirtualGuestMonotonicClock {
    fn resolution_nanoseconds(&self) -> u64 {
        1
    }

    fn now_nanoseconds(&self) -> u64 {
        let mut now = self
            .now_nanoseconds
            .lock()
            .expect("virtual guest-clock mutex is not poisoned");
        let value = *now;
        *now = now.saturating_add(self.step_nanoseconds);
        value
    }
}

#[derive(Clone)]
pub struct RuntimeTimeSeams {
    pub deadline_clock: Arc<dyn youth_state::DeadlineClock>,
    pub wake_driver: Arc<dyn youth_state::WakeDriver>,
    pub guest_monotonic_clock: Arc<dyn GuestMonotonicClock>,
    pub notification_dispatcher: Arc<dyn NotificationDispatcher>,
    pub clipboard_service: Arc<dyn ClipboardService>,
}

impl Default for RuntimeTimeSeams {
    fn default() -> Self {
        Self {
            deadline_clock: Arc::new(youth_state::SystemDeadlineClock),
            wake_driver: Arc::new(youth_state::SystemWakeDriver::default()),
            guest_monotonic_clock: Arc::new(SystemGuestMonotonicClock::default()),
            notification_dispatcher: Arc::new(SystemNotificationDispatcher),
            clipboard_service: Arc::new(SystemClipboardService::new()),
        }
    }
}

impl fmt::Debug for RuntimeTimeSeams {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeTimeSeams")
            .finish_non_exhaustive()
    }
}

impl PartialEq for RuntimeTimeSeams {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.deadline_clock, &other.deadline_clock)
            && Arc::ptr_eq(&self.wake_driver, &other.wake_driver)
            && Arc::ptr_eq(&self.guest_monotonic_clock, &other.guest_monotonic_clock)
            && Arc::ptr_eq(
                &self.notification_dispatcher,
                &other.notification_dispatcher,
            )
            && Arc::ptr_eq(&self.clipboard_service, &other.clipboard_service)
    }
}

impl Eq for RuntimeTimeSeams {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct YouthAppConfig {
    pub component_path: PathBuf,
    pub app_id: AppId,
    pub state: StateLocation,
    pub limits: RuntimeLimits,
}

impl YouthAppConfig {
    pub fn ephemeral(component_path: impl AsRef<Path>) -> Self {
        Self {
            component_path: component_path.as_ref().to_owned(),
            app_id: AppId::parse("dev.youth.ephemeral")
                .expect("the built-in ephemeral application ID is valid"),
            state: StateLocation::Memory,
            limits: RuntimeLimits::default(),
        }
    }

    #[must_use]
    pub fn with_time_seams(mut self, time: RuntimeTimeSeams) -> Self {
        self.limits.time = time;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clipboard_construction_is_infallible_and_never_panics() {
        // Deliberately does not exercise read_text/write_text here: those
        // touch the real OS pasteboard, which would clobber whatever a
        // developer running this test suite currently has copied. The
        // graceful-degradation behavior when a platform clipboard is
        // unavailable (headless CI, no display server) is exercised by
        // construction alone succeeding regardless of environment.
        let clipboard = SystemClipboardService::new();
        let _ = format!("{clipboard:?}");
    }
}
