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

/// Placeholder clipboard used until A7 supplies native OS integration.
/// Reads behave as an empty clipboard and writes are intentionally discarded.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClipboardService;

impl ClipboardService for SystemClipboardService {
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        Ok(None)
    }

    fn write_text(&self, _text: &str) -> Result<(), ClipboardError> {
        Ok(())
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
            clipboard_service: Arc::new(SystemClipboardService),
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
