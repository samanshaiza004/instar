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
        Self {
            max_component_size: 32 * 1024 * 1024,
            max_linear_memory: 128 * 1024 * 1024,
            max_table_elements: 1_000_000,
            max_event_batch: 256,
            max_guest_error_message: 4 * 1024,
            max_guest_to_host_transfer: 8 * 1024 * 1024,
            max_ime_preedit_bytes: 16 * 1024,
            tree: youth_tree::Limits::default(),
            mount: CallBudget {
                fuel: 100_000_000,
                deadline: Duration::from_millis(500),
            },
            handle: CallBudget {
                fuel: 20_000_000,
                deadline: Duration::from_millis(100),
            },
            resync: CallBudget {
                fuel: 100_000_000,
                deadline: Duration::from_millis(500),
            },
            state: youth_state::StateLimits::default(),
            time: crate::config::RuntimeTimeSeams::default(),
        }
    }
}
