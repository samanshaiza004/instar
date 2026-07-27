use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::WakeToken;

pub trait DeadlineClock: Send + Sync {
    fn now_epoch_millis(&self) -> u64;
}

pub trait WakeDriver: Send + Sync {
    fn arm(&self, token: WakeToken, delay: Duration);
    fn cancel(&self, token: WakeToken);
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

#[derive(Clone, Debug, Default)]
pub struct SystemWakeDriver {
    state: Arc<Mutex<SystemWakeState>>,
}

#[derive(Debug, Default)]
struct SystemWakeState {
    next_arm: u64,
    armed: HashMap<WakeToken, u64>,
    received: Vec<WakeToken>,
}

impl SystemWakeDriver {
    #[must_use]
    pub fn take_received(&self) -> Vec<WakeToken> {
        std::mem::take(
            &mut self
                .state
                .lock()
                .expect("system wake-driver mutex is not poisoned")
                .received,
        )
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
            state.armed.insert(token, arm);
            arm
        };
        let state = Arc::clone(&self.state);
        std::thread::spawn(move || {
            // Duration sleeping is process-local and monotonic, so wall-clock
            // rollback cannot extend an already armed timer.
            std::thread::sleep(delay);
            let mut state = state
                .lock()
                .expect("system wake-driver mutex is not poisoned");
            if state.armed.get(&token) == Some(&arm) {
                state.armed.remove(&token);
                state.received.push(token);
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

#[derive(Clone, Debug, Default)]
pub struct VirtualWakeDriver {
    state: Arc<Mutex<VirtualWakeState>>,
}

#[derive(Debug, Default)]
struct VirtualWakeState {
    now: Duration,
    armed: HashMap<WakeToken, Duration>,
    received: Vec<WakeToken>,
    arm_count: usize,
}

impl VirtualWakeDriver {
    pub fn advance(&self, duration: Duration) {
        let mut state = self
            .state
            .lock()
            .expect("virtual wake-driver mutex is not poisoned");
        state.now = state.now.saturating_add(duration);
        let now = state.now;
        let mut due: Vec<_> = state
            .armed
            .iter()
            .filter_map(|(token, deadline)| (*deadline <= now).then_some(*token))
            .collect();
        due.sort_by_key(|token| (token.schedule_id, token.generation));
        for token in due {
            state.armed.remove(&token);
            state.received.push(token);
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
            .map(|(token, deadline)| (*token, deadline.saturating_sub(state.now)))
            .collect();
        values.sort_by_key(|(token, _)| (token.schedule_id, token.generation));
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
}

pub fn execute_wake_outputs(driver: &dyn WakeDriver, outputs: &[crate::SchedulerOutput]) {
    for output in outputs {
        match output {
            crate::SchedulerOutput::ArmWake { token, delay } => driver.arm(*token, *delay),
            crate::SchedulerOutput::CancelWake(token) => driver.cancel(*token),
            crate::SchedulerOutput::PersistMutation(_)
            | crate::SchedulerOutput::QueueElapsedDelivery { .. }
            | crate::SchedulerOutput::DiscardStaleWake(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_clock_rollback_does_not_extend_an_armed_monotonic_wake() {
        let wall = VirtualDeadlineClock::new(1_000);
        let wakes = VirtualWakeDriver::default();
        let token = WakeToken {
            schedule_id: 1,
            generation: 1,
        };
        wakes.arm(token, Duration::from_millis(100));
        wall.set(100);
        wakes.advance(Duration::from_millis(100));
        assert_eq!(wall.now_epoch_millis(), 100);
        assert_eq!(wakes.take_received(), vec![token]);
    }
}
