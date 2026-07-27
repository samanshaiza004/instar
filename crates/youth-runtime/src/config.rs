use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

pub use youth_state::{AppId, StateLocation};

use crate::RuntimeLimits;

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
}

impl Default for RuntimeTimeSeams {
    fn default() -> Self {
        Self {
            deadline_clock: Arc::new(youth_state::SystemDeadlineClock),
            wake_driver: Arc::new(youth_state::SystemWakeDriver::default()),
            guest_monotonic_clock: Arc::new(SystemGuestMonotonicClock::default()),
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
