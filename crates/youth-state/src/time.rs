use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::WakeToken;

pub trait DeadlineClock: Send + Sync {
    fn now_epoch_millis(&self) -> u64;
}

pub trait WakeSink: Send + Sync {
    fn push(&self, token: WakeToken);
}

pub trait WakeDriver: Send + Sync {
    fn arm(&self, token: WakeToken, delay: Duration);
    fn cancel(&self, token: WakeToken);
    fn set_sink(&self, sink: Arc<dyn WakeSink>);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemDeadlineClock;

impl DeadlineClock for SystemDeadlineClock {
    fn now_epoch_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok())
            .unwrap_or(0)
    }
}

#[derive(Clone, Default)]
pub struct SystemWakeDriver {
    state: Arc<Mutex<SystemWakeState>>,
}

#[derive(Default)]
struct SystemWakeState {
    next_arm: u64,
    armed: HashMap<WakeToken, u64>,
    sink: Option<Arc<dyn WakeSink>>,
}

impl std::fmt::Debug for SystemWakeDriver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SystemWakeDriver")
            .finish_non_exhaustive()
    }
}

impl WakeDriver for SystemWakeDriver {
    fn arm(&self, token: WakeToken, delay: Duration) {
        let arm = {
            let mut state = self
                .state
                .lock()
                .expect("system wake-driver mutex is not poisoned");
            state.next_arm = state.next_arm.wrapping_add(1);
            let arm = state.next_arm;
            state.armed.insert(token.clone(), arm);
            arm
        };
        let state = Arc::clone(&self.state);
        std::thread::spawn(move || {
            // Duration sleeping is process-local and monotonic, so wall-clock
            // rollback cannot extend an already armed timer.
            std::thread::sleep(delay);
            let sink = {
                let mut state = state
                    .lock()
                    .expect("system wake-driver mutex is not poisoned");
                if state.armed.get(&token) != Some(&arm) {
                    return;
                }
                state.armed.remove(&token);
                state.sink.clone()
            };
            if let Some(sink) = sink {
                sink.push(token);
            }
        });
    }

    fn cancel(&self, token: WakeToken) {
        self.state
            .lock()
            .expect("system wake-driver mutex is not poisoned")
            .armed
            .remove(&token);
    }

    fn set_sink(&self, sink: Arc<dyn WakeSink>) {
        self.state
            .lock()
            .expect("system wake-driver mutex is not poisoned")
            .sink = Some(sink);
    }
}

#[derive(Clone, Debug, Default)]
pub struct VirtualDeadlineClock {
    now_epoch_millis: Arc<Mutex<u64>>,
}

impl VirtualDeadlineClock {
    #[must_use]
    pub fn new(now_epoch_millis: u64) -> Self {
        Self {
            now_epoch_millis: Arc::new(Mutex::new(now_epoch_millis)),
        }
    }

    pub fn set(&self, now_epoch_millis: u64) {
        *self
            .now_epoch_millis
            .lock()
            .expect("virtual deadline-clock mutex is not poisoned") = now_epoch_millis;
    }

    pub fn advance(&self, duration: Duration) {
        let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        let mut now = self
            .now_epoch_millis
            .lock()
            .expect("virtual deadline-clock mutex is not poisoned");
        *now = now.saturating_add(millis);
    }
}

impl DeadlineClock for VirtualDeadlineClock {
    fn now_epoch_millis(&self) -> u64 {
        *self
            .now_epoch_millis
            .lock()
            .expect("virtual deadline-clock mutex is not poisoned")
    }
}

#[derive(Clone, Default)]
pub struct VirtualWakeDriver {
    state: Arc<Mutex<VirtualWakeState>>,
}

#[derive(Default)]
struct VirtualWakeState {
    now: Duration,
    armed: HashMap<WakeToken, Duration>,
    sink: Option<Arc<dyn WakeSink>>,
    received: Vec<WakeToken>,
    arm_count: usize,
}

impl std::fmt::Debug for VirtualWakeDriver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VirtualWakeDriver")
            .finish_non_exhaustive()
    }
}

impl VirtualWakeDriver {
    /// Advances only virtual monotonic time. Tests call [`Self::fire`] to
    /// deterministically choose where the wake enters the FIFO mailbox.
    pub fn advance(&self, duration: Duration) {
        let mut state = self
            .state
            .lock()
            .expect("virtual wake-driver mutex is not poisoned");
        state.now = state.now.saturating_add(duration);
    }

    #[must_use]
    pub fn due(&self) -> Vec<WakeToken> {
        let state = self
            .state
            .lock()
            .expect("virtual wake-driver mutex is not poisoned");
        let mut due: Vec<_> = state
            .armed
            .iter()
            .filter_map(|(token, deadline)| (*deadline <= state.now).then_some(token.clone()))
            .collect();
        sort_tokens(&mut due);
        due
    }

    /// Fires one currently armed and due token.
    #[must_use]
    pub fn fire(&self, token: &WakeToken) -> bool {
        let sink = {
            let mut state = self
                .state
                .lock()
                .expect("virtual wake-driver mutex is not poisoned");
            if state
                .armed
                .get(token)
                .is_none_or(|deadline| *deadline > state.now)
            {
                return false;
            }
            state.armed.remove(token);
            state.sink.clone()
        };
        if let Some(sink) = sink {
            std::thread::scope(|scope| {
                scope
                    .spawn(|| sink.push(token.clone()))
                    .join()
                    .expect("virtual wake sink thread does not panic");
            });
        } else {
            self.state
                .lock()
                .expect("virtual wake-driver mutex is not poisoned")
                .received
                .push(token.clone());
        }
        true
    }

    /// Pushes an arbitrary token for repeated and stale-wake tests.
    pub fn fire_stale(&self, token: WakeToken) {
        let sink = self
            .state
            .lock()
            .expect("virtual wake-driver mutex is not poisoned")
            .sink
            .clone();
        if let Some(sink) = sink {
            std::thread::scope(|scope| {
                scope
                    .spawn(|| sink.push(token))
                    .join()
                    .expect("virtual wake sink thread does not panic");
            });
        } else {
            self.state
                .lock()
                .expect("virtual wake-driver mutex is not poisoned")
                .received
                .push(token);
        }
    }

    #[must_use]
    pub fn take_received(&self) -> Vec<WakeToken> {
        std::mem::take(
            &mut self
                .state
                .lock()
                .expect("virtual wake-driver mutex is not poisoned")
                .received,
        )
    }

    #[must_use]
    pub fn armed(&self) -> Vec<(WakeToken, Duration)> {
        let state = self
            .state
            .lock()
            .expect("virtual wake-driver mutex is not poisoned");
        let mut values: Vec<_> = state
            .armed
            .iter()
            .map(|(token, deadline)| (token.clone(), deadline.saturating_sub(state.now)))
            .collect();
        values.sort_by(|(left, _), (right, _)| token_key(left).cmp(&token_key(right)));
        values
    }

    #[must_use]
    pub fn arm_count(&self) -> usize {
        self.state
            .lock()
            .expect("virtual wake-driver mutex is not poisoned")
            .arm_count
    }
}

impl WakeDriver for VirtualWakeDriver {
    fn arm(&self, token: WakeToken, delay: Duration) {
        let mut state = self
            .state
            .lock()
            .expect("virtual wake-driver mutex is not poisoned");
        let deadline = state.now.saturating_add(delay);
        state.armed.insert(token, deadline);
        state.arm_count += 1;
    }

    fn cancel(&self, token: WakeToken) {
        self.state
            .lock()
            .expect("virtual wake-driver mutex is not poisoned")
            .armed
            .remove(&token);
    }

    fn set_sink(&self, sink: Arc<dyn WakeSink>) {
        self.state
            .lock()
            .expect("virtual wake-driver mutex is not poisoned")
            .sink = Some(sink);
    }
}

fn token_key(token: &WakeToken) -> (&crate::AppId, u64, u64) {
    (&token.app_id, token.schedule_id, token.generation)
}

fn sort_tokens(tokens: &mut [WakeToken]) {
    tokens.sort_by(|left, right| token_key(left).cmp(&token_key(right)));
}

pub fn execute_wake_outputs(driver: &dyn WakeDriver, outputs: &[crate::SchedulerOutput]) {
    for output in outputs {
        match output {
            crate::SchedulerOutput::ArmWake { token, delay } => {
                driver.arm(token.clone(), *delay);
            }
            crate::SchedulerOutput::CancelWake(token) => driver.cancel(token.clone()),
            crate::SchedulerOutput::PersistMutation(_)
            | crate::SchedulerOutput::QueueElapsedDelivery { .. }
            | crate::SchedulerOutput::DiscardStaleWake(_) => {}
        }
    }
}
