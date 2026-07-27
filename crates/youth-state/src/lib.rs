//! Host-owned typed application state for Youth.

#![forbid(unsafe_code)]

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use thiserror::Error;

mod scheduler;
mod store;
mod time;

pub use scheduler::{SchedulerInput, SchedulerOutput, WakeToken, transition};
pub use store::{
    DeliveryProtocol, ElapsedReason, GuestCallPhase, PendingDelivery, ScheduleRecord,
    ScheduleStatus, StateError, StateStore, TurnStateMetrics, Usage, Verification, repair_usage,
    verify_file,
};
pub use time::{
    DeadlineClock, SystemDeadlineClock, SystemWakeDriver, VirtualDeadlineClock, VirtualWakeDriver,
    WakeDriver, execute_wake_outputs,
};

/// Stable application identity used to select durable state.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AppId(String);

impl AppId {
    pub fn parse(value: impl Into<String>) -> Result<Self, AppIdError> {
        let value = value.into();
        validate_app_id(&value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AppId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for AppId {
    type Err = AppIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid application ID: {reason}")]
pub struct AppIdError {
    reason: &'static str,
}

fn validate_app_id(value: &str) -> Result<(), AppIdError> {
    let bytes = value.as_bytes();
    let invalid = |reason| Err(AppIdError { reason });
    if bytes.is_empty() || bytes.len() > 255 {
        return invalid("length must be between 1 and 255 bytes");
    }
    if !bytes[0].is_ascii_lowercase() {
        return invalid("the first byte must be an ASCII lowercase letter");
    }
    if value.starts_with('.') || value.ends_with('.') || value.contains("..") {
        return invalid("dots may not lead, trail, or occur consecutively");
    }
    if !bytes.iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
    }) {
        return invalid("only lowercase ASCII letters, digits, '.', '-', and '_' are allowed");
    }
    Ok(())
}

/// Exact values exposed through the state WIT interface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StateValue {
    Boolean(bool),
    Integer(i64),
    Text(String),
    Bytes(Vec<u8>),
}

impl StateValue {
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Boolean(_) => 1,
            Self::Integer(_) => 8,
            Self::Text(value) => value.len(),
            Self::Bytes(value) => value.len(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StateLocation {
    Memory,
    File(PathBuf),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StateLimits {
    pub max_key_bytes: usize,
    pub max_text_bytes: usize,
    pub max_bytes_bytes: usize,
    pub max_total_bytes: u64,
    pub max_keys: u32,
    pub max_writes_per_turn: u32,
    pub max_calls_per_turn: u32,
    pub max_active_schedules: usize,
    pub min_schedule_millis: u64,
    pub max_schedule_millis: u64,
    pub max_notification_title_bytes: usize,
    pub max_notification_body_bytes: usize,
}

impl Default for StateLimits {
    fn default() -> Self {
        Self {
            max_key_bytes: 256,
            max_text_bytes: 1024 * 1024,
            max_bytes_bytes: 1024 * 1024,
            max_total_bytes: 16 * 1024 * 1024,
            max_keys: 16_384,
            max_writes_per_turn: 1_024,
            max_calls_per_turn: 4_096,
            max_active_schedules: 32,
            min_schedule_millis: 100,
            max_schedule_millis: 30 * 24 * 60 * 60 * 1_000,
            max_notification_title_bytes: 256,
            max_notification_body_bytes: 1_024,
        }
    }
}

pub const ENTRY_OVERHEAD_BYTES: u64 = 32;

pub fn logical_entry_bytes(key: &str, value: &StateValue) -> Result<u64, StateModelError> {
    let key = u64::try_from(key.len()).map_err(|_| StateModelError::Overflow)?;
    let value = u64::try_from(value.encoded_len()).map_err(|_| StateModelError::Overflow)?;
    key.checked_add(value)
        .and_then(|total| total.checked_add(ENTRY_OVERHEAD_BYTES))
        .ok_or(StateModelError::Overflow)
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum StateModelError {
    #[error("logical state size overflow")]
    Overflow,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StateSummary {
    pub schema_version: u32,
    pub key_count: u32,
    pub logical_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ids_are_path_safe() {
        for valid in ["a", "dev.youth.counter", "a-b_c9"] {
            assert_eq!(AppId::parse(valid).unwrap().as_str(), valid);
        }
        for invalid in [
            "", ".a", "a.", "a..b", "A", "9app", "a/b", "a\\b", "é", "a b",
        ] {
            assert!(AppId::parse(invalid).is_err(), "accepted {invalid:?}");
        }
        assert!(AppId::parse(format!("a{}", "b".repeat(254))).is_ok());
        assert!(AppId::parse(format!("a{}", "b".repeat(255))).is_err());
    }

    #[test]
    fn logical_size_counts_key_value_and_overhead() {
        assert_eq!(
            logical_entry_bytes("count", &StateValue::Integer(0)),
            Ok(45)
        );
        assert_eq!(
            logical_entry_bytes("flag", &StateValue::Boolean(true)),
            Ok(37)
        );
    }

    #[test]
    fn default_limits_match_milestone_contract() {
        let limits = StateLimits::default();
        assert_eq!(limits.max_keys, 16_384);
        assert_eq!(limits.max_total_bytes, 16 * 1024 * 1024);
        assert_eq!(limits.max_writes_per_turn, 1_024);
        assert_eq!(limits.max_calls_per_turn, 4_096);
        assert_eq!(limits.max_active_schedules, 32);
        assert_eq!(limits.min_schedule_millis, 100);
        assert_eq!(limits.max_schedule_millis, 30 * 24 * 60 * 60 * 1_000);
        assert_eq!(limits.max_notification_title_bytes, 256);
        assert_eq!(limits.max_notification_body_bytes, 1_024);
    }
}
