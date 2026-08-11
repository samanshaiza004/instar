//! Instar's one explicit runtime resource policy (HARDEN-3).
//!
//! Instar deliberately has **one** policy for every guest generation, defined
//! here as [`ResourcePolicy::instar_default`]. No caller may scatter raw
//! Wasmtime limits around the codebase; if a limit is needed it is a field of
//! this policy and is installed by [`MeasuredLimiter`] on every `Store`.
//!
//! The policy is containment, not a CPU budget. It bounds what a guest can
//! hold at one time -- linear-memory bytes per memory, table elements per
//! table, and the counts of memories, tables, and core instances -- and leaves
//! CPU scheduling alone. Instar does not recreate Youth's quota scheduler,
//! fuel refill system, or periodic epoch thread; killability for non-yielding
//! Wasm is the separate epoch mechanism in `crate::engine` and
//! `crate::runtime`.
//!
//! # Measurement method
//!
//! Wasmtime 47 does not expose a current-count metric for instances, memories,
//! or tables. The evidence recorded here therefore uses two defensible
//! methods, both explained next to the code that collects them:
//!
//! * Memory and table counts come from
//!   [`Component::resources_required`](wasmtime::component::Component::resources_required),
//!   Wasmtime's supported static resource summary. Limiter callbacks record
//!   the largest accepted memory/table size reached at runtime.
//! * Core instances are measured by **limit probing** in
//!   `crates/instar-host/tests/resource_policy.rs`: a component's instance
//!   demand is the smallest `instances` limit under which a fresh Store
//!   instantiates it. There is no per-instance limiter callback in Wasmtime
//!   47, and the public store API offers no instance enumerator without the
//!   optional `debug` Cargo feature, so the probe is the existing API's honest
//!   way to establish that number.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use wasmtime::component::Component;
use wasmtime::{ResourceLimiter, StoreLimits, StoreLimitsBuilder};

/// How many ticks beyond the current epoch a generation's epoch deadline is.
///
/// The runtime increments the engine epoch only as an out-of-band shutdown
/// signal, so one tick is the entire killability budget: after
/// `Engine::increment_epoch` fires, any currently executing Wasm in that
/// generation has reached its deadline and traps at the next epoch check. No
/// periodic ticker exists; idle guests parked in async host imports are never
/// touched by epochs (Wasmtime only checks epochs while Wasm is executing).
pub const EPOCH_DEADLINE_TICKS: u64 = 1;

/// The resource limits every Instar generation runs under.
///
/// Fields are deliberately named for the policy, not for Wasmtime: the policy
/// is the contract, and `StoreLimits` is its installation detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourcePolicy {
    /// Maximum bytes one linear memory may reach. Per-memory: Wasmtime's
    /// `StoreLimits` has no aggregate-linear-memory bound in this version.
    pub memory_bytes: usize,
    /// Maximum elements one table may reach.
    pub table_elements: usize,
    /// Maximum core instances live in one Store at once.
    pub instances: usize,
    /// Maximum linear memories live in one Store at once.
    pub memories: usize,
    /// Maximum tables live in one Store at once.
    pub tables: usize,
    /// Grow past a limit traps instead of returning `-1`/`None` to the guest.
    ///
    /// True on purpose: a refusal that looks like an ordinary allocation
    /// failure lets a confused guest continue on a path it believes worked,
    /// while a trap is an unambiguous guest failure the host observes and
    /// contains.
    pub trap_on_grow_failure: bool,
}
impl ResourcePolicy {
    /// The single policy all Instar generations use.
    ///
    /// The values are calibrated against the five-guest measurement table in
    /// `crates/instar-host/tests/resource_policy.rs`; every guest fits by a
    /// wide margin and the table fails loudly if a guest drifts toward the
    /// policy.
    pub fn instar_default() -> Self {
        Self {
            memory_bytes: 64 * 1024 * 1024,
            table_elements: 10_000,
            instances: 16,
            memories: 4,
            tables: 8,
            trap_on_grow_failure: true,
        }
    }

    /// A copy with a different core-instance ceiling.
    ///
    /// Used only by the measurement probe that establishes a component's
    /// instance demand; production always uses [`ResourcePolicy::instar_default`].
    pub fn with_instances(mut self, instances: usize) -> Self {
        self.instances = instances;
        self
    }

    /// Installs the policy into a Wasmtime store-limits value.
    pub fn to_store_limits(&self) -> StoreLimits {
        StoreLimitsBuilder::new()
            .memory_size(self.memory_bytes)
            .table_elements(self.table_elements)
            .instances(self.instances)
            .memories(self.memories)
            .tables(self.tables)
            .trap_on_grow_failure(self.trap_on_grow_failure)
            .build()
    }
}

/// Measured resource evidence for one generation's instantiate/run startup.
///
/// All fields are atomics because Wasmtime invokes the limiter from the Wasm
/// execution path while tests read it from another thread.
#[derive(Debug, Default)]
pub struct ResourceMetrics {
    /// Linear memories required by the compiled component.
    memories_required: AtomicU64,
    /// Tables required by the compiled component.
    tables_required: AtomicU64,
    /// Highest linear-memory size Wasmtime was asked to reach, in bytes.
    peak_memory_bytes: AtomicU64,
    /// Highest table size Wasmtime was asked to reach, in elements.
    peak_table_elements: AtomicU64,
}

/// A point-in-time copy of [`ResourceMetrics`] for assertions and reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceMetricsSnapshot {
    pub memories_required: u64,
    pub tables_required: u64,
    pub peak_memory_bytes: u64,
    pub peak_table_elements: u64,
}

impl ResourceMetrics {
    /// Starts a measurement from Wasmtime's supported component-resource
    /// summary. Components with dynamic core imports have no static summary;
    /// their counts remain zero while runtime peaks are still observed.
    pub fn for_component(component: &Component) -> Self {
        let Some(required) = component.resources_required() else {
            return Self::default();
        };
        Self {
            memories_required: AtomicU64::new(u64::from(required.num_memories)),
            tables_required: AtomicU64::new(u64::from(required.num_tables)),
            peak_memory_bytes: AtomicU64::default(),
            peak_table_elements: AtomicU64::default(),
        }
    }

    pub fn snapshot(&self) -> ResourceMetricsSnapshot {
        ResourceMetricsSnapshot {
            memories_required: self.memories_required.load(Ordering::SeqCst),
            tables_required: self.tables_required.load(Ordering::SeqCst),
            peak_memory_bytes: self.peak_memory_bytes.load(Ordering::SeqCst),
            peak_table_elements: self.peak_table_elements.load(Ordering::SeqCst),
        }
    }

    fn record_memory(&self, desired: usize) {
        self.peak_memory_bytes
            .fetch_max(desired as u64, Ordering::SeqCst);
    }

    fn record_table(&self, desired: usize) {
        self.peak_table_elements
            .fetch_max(desired as u64, Ordering::SeqCst);
    }
}

/// The policy installed on one Store, recording resource evidence as it runs.
///
/// Lives in the store's `T` state and is handed to Wasmtime via
/// `Store::limiter(|state| &mut state.limits)`, so every memory/table creation
/// and growth in the generation is measured with the same object that enforces
/// the policy.
pub struct MeasuredLimiter {
    limits: StoreLimits,
    metrics: Arc<ResourceMetrics>,
}

impl MeasuredLimiter {
    pub fn new(policy: &ResourcePolicy, metrics: Arc<ResourceMetrics>) -> Self {
        Self {
            limits: policy.to_store_limits(),
            metrics,
        }
    }

    pub fn metrics(&self) -> Arc<ResourceMetrics> {
        Arc::clone(&self.metrics)
    }
}

impl ResourceLimiter for MeasuredLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allowed = self.limits.memory_growing(current, desired, maximum);
        if matches!(allowed, Ok(true)) {
            self.metrics.record_memory(desired);
        }
        allowed
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.limits.memory_grow_failed(error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allowed = self.limits.table_growing(current, desired, maximum);
        if matches!(allowed, Ok(true)) {
            self.metrics.record_table(desired);
        }
        allowed
    }

    fn table_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.limits.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.limits.instances()
    }

    fn tables(&self) -> usize {
        self.limits.tables()
    }

    fn memories(&self) -> usize {
        self.limits.memories()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_rejects_memory_growth_over_the_limit_and_metrics_record_peaks() {
        let metrics = Arc::new(ResourceMetrics::default());
        let mut limiter = MeasuredLimiter::new(&ResourcePolicy::instar_default(), metrics);

        // A normal startup-size memory is accepted and recorded.
        assert!(
            limiter
                .memory_growing(0, 4 * 1024 * 1024, None)
                .expect("policy allows")
        );
        assert!(
            limiter
                .memory_growing(4 * 1024 * 1024, 8 * 1024 * 1024, None)
                .expect("policy allows")
        );
        assert!(
            limiter
                .memory_growing(8 * 1024 * 1024, 16 * 1024 * 1024, None)
                .expect("policy allows")
        );

        // Past the policy, the limiter traps on growth failure as configured.
        let over = ResourcePolicy::instar_default().memory_bytes + 1;
        let result = limiter.memory_growing(16 * 1024 * 1024, over, None);
        assert!(
            result.is_err(),
            "growth past the policy must be refused, not silently allowed"
        );

        let snapshot = limiter.metrics().snapshot();
        assert_eq!(snapshot.memories_required, 0);
        assert_eq!(snapshot.peak_memory_bytes, 16 * 1024 * 1024);
    }

    #[test]
    fn policy_rejects_table_growth_over_the_limit() {
        let metrics = Arc::new(ResourceMetrics::default());
        let mut limiter = MeasuredLimiter::new(&ResourcePolicy::instar_default(), metrics);

        assert!(
            limiter
                .table_growing(0, 1_024, None)
                .expect("policy allows")
        );
        assert!(
            limiter
                .table_growing(1_024, 2_048, None)
                .expect("policy allows")
        );

        let over = ResourcePolicy::instar_default().table_elements + 1;
        assert!(
            limiter.table_growing(2_048, over, None).is_err(),
            "table growth past the policy must be refused"
        );

        let snapshot = limiter.metrics().snapshot();
        assert_eq!(snapshot.tables_required, 0);
        assert_eq!(snapshot.peak_table_elements, 2_048);
    }

    #[test]
    fn policy_count_limits_are_what_store_limits_will_enforce() {
        let policy = ResourcePolicy::instar_default();
        let limits = policy.to_store_limits();
        assert_eq!(limits.instances(), policy.instances);
        assert_eq!(limits.memories(), policy.memories);
        assert_eq!(limits.tables(), policy.tables);
    }
}
