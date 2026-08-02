use std::time::Duration;

/// Fuel and wall-clock allowance for one synchronous guest call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CallBudget {
    pub fuel: u64,
    pub deadline: Duration,
}

/// Configurable Milestone 0 containment and protocol limits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeLimits {
    pub max_component_size: usize,
    pub max_linear_memory: usize,
    pub max_table_elements: usize,
    pub max_event_batch: usize,
    pub max_guest_error_message: usize,
    pub max_guest_to_host_transfer: usize,
    /// Maximum UTF-8 byte length of an in-progress host IME preedit.
    /// Committed Editor text is bounded by `tree.max_editor_text_len`.
    pub max_ime_preedit_bytes: usize,
    pub tree: youth_tree::Limits,
    pub mount: CallBudget,
    pub handle: CallBudget,
    pub resync: CallBudget,
    pub state: youth_state::StateLimits,
    /// Runtime-owned time dependencies, carried here so adding B-3 remains
    /// source-compatible with existing `YouthAppConfig` struct literals.
    pub time: crate::config::RuntimeTimeSeams,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        let scale = call_deadline_scale(std::env::var("YOUTH_CALL_DEADLINE_SCALE").ok().as_deref());
        Self {
            max_component_size: 32 * 1024 * 1024,
            max_linear_memory: 128 * 1024 * 1024,
            max_table_elements: 1_000_000,
            max_event_batch: 256,
            max_guest_error_message: 4 * 1024,
            max_guest_to_host_transfer: 8 * 1024 * 1024,
            max_ime_preedit_bytes: 16 * 1024,
            tree: youth_tree::Limits::default(),
            mount: scaled_call_budget(
                CallBudget {
                    fuel: 100_000_000,
                    deadline: Duration::from_millis(500),
                },
                scale,
            ),
            handle: scaled_call_budget(
                CallBudget {
                    fuel: 20_000_000,
                    deadline: Duration::from_millis(100),
                },
                scale,
            ),
            resync: scaled_call_budget(
                CallBudget {
                    fuel: 100_000_000,
                    deadline: Duration::from_millis(500),
                },
                scale,
            ),
            state: youth_state::StateLimits::default(),
            time: crate::config::RuntimeTimeSeams::default(),
        }
    }
}

/// Multiplies a `CallBudget`'s wall-clock deadline; `fuel` is untouched
/// because it already bounds a runaway guest independent of host speed.
fn scaled_call_budget(base: CallBudget, scale: u32) -> CallBudget {
    CallBudget {
        fuel: base.fuel,
        deadline: base.deadline * scale,
    }
}

/// Reads `YOUTH_CALL_DEADLINE_SCALE`, an escape hatch for contended CI
/// runners -- observed repeatedly on windows-latest, where a legitimate
/// guest call (real SQLite I/O, real Parley layout) occasionally exceeds
/// the tight production deadline under scheduling load, tripping
/// `DeadlineExceeded` as a false positive rather than catching a genuinely
/// runaway guest. Set only by .github/workflows/ci.yml's windows-latest
/// leg; unset (and therefore a no-op scale of 1) everywhere else,
/// including every real Youth install, so production containment stays
/// exactly as tight as before.
fn call_deadline_scale(raw: Option<&str>) -> u32 {
    raw.and_then(|value| value.parse::<u32>().ok())
        .filter(|&scale| scale >= 1)
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_deadline_scale_defaults_to_one_when_unset_or_invalid() {
        assert_eq!(call_deadline_scale(None), 1);
        assert_eq!(call_deadline_scale(Some("")), 1);
        assert_eq!(call_deadline_scale(Some("0")), 1);
        assert_eq!(call_deadline_scale(Some("-3")), 1);
        assert_eq!(call_deadline_scale(Some("not-a-number")), 1);
    }

    #[test]
    fn call_deadline_scale_reads_a_positive_integer() {
        assert_eq!(call_deadline_scale(Some("5")), 5);
    }

    #[test]
    fn scaled_call_budget_multiplies_the_deadline_and_leaves_fuel_untouched() {
        let base = CallBudget {
            fuel: 42,
            deadline: Duration::from_millis(100),
        };
        let scaled = scaled_call_budget(base, 5);
        assert_eq!(scaled.fuel, 42);
        assert_eq!(scaled.deadline, Duration::from_millis(500));
    }
}
