use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use tracing::info_span;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, ResourceLimiter, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bindings::{Application, ApplicationPre};
use crate::engine::deadline_ticks;
use crate::error::ErrorContext;
use crate::wire::from_guest::{self, WireErrorKind};
use crate::wire::to_guest::{self, HostEvent};
use crate::{CallBudget, RuntimeError, RuntimeErrorCategory, RuntimeLimits};

const APPLICATION_WORLD: &str = "youth:app/application@0.0.1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppLifecycle {
    Loaded,
    Mounted,
    Faulted,
    Stopped,
}

impl fmt::Display for AppLifecycle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Loaded => "loaded",
            Self::Mounted => "mounted",
            Self::Faulted => "faulted",
            Self::Stopped => "stopped",
        })
    }
}

/// Metrics and protocol coordinates for a committed event turn.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnReceipt {
    pub turn_id: u64,
    pub event_sequence: u64,
    pub base_revision: u64,
    pub next_revision: u64,
    pub patch_count: usize,
    pub committed: bool,
}

/// Stable information retained when an application becomes faulted.
#[derive(Clone, Debug)]
pub struct AppFault {
    pub category: RuntimeErrorCategory,
    pub message: String,
}

/// A synchronous snapshot of host-owned application state.
#[derive(Clone, Debug)]
pub struct AppInspection {
    pub lifecycle: AppLifecycle,
    pub world: String,
    pub current_revision: Option<u64>,
    pub next_event_sequence: Option<u64>,
    pub last_event_sequence: Option<u64>,
    pub node_count: usize,
    pub depth: usize,
    pub last_turn: Option<TurnReceipt>,
    pub fault: Option<AppFault>,
    pub canonical_tree: String,
}

struct MemoryLimiter {
    maximum: usize,
    max_table_elements: usize,
    limit_hit: bool,
}

impl ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allowed = desired <= self.maximum;
        self.limit_hit |= !allowed;
        Ok(allowed)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allowed = desired <= self.max_table_elements;
        self.limit_hit |= !allowed;
        Ok(allowed)
    }
}

struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
    limiter: MemoryLimiter,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// One synchronous, single-owner Youth component instance.
pub struct YouthApp {
    component_id: String,
    limits: RuntimeLimits,
    store: Store<HostState>,
    bindings: Application,
    tree: Option<youth_tree::Tree>,
    lifecycle: AppLifecycle,
    last_event_sequence: Option<u64>,
    last_turn: Option<TurnReceipt>,
    fault: Option<AppFault>,
}

impl fmt::Debug for YouthApp {
    /// Reports identity and protocol state only. Tree payloads are
    /// excluded so that debug output never leaks node contents
    /// (spec section 18).
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("YouthApp")
            .field("component_id", &self.component_id)
            .field("lifecycle", &self.lifecycle)
            .field(
                "revision",
                &self.tree.as_ref().map(youth_tree::Tree::revision),
            )
            .finish_non_exhaustive()
    }
}

impl YouthApp {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        Self::load_with_limits(path, RuntimeLimits::default())
    }

    pub fn load_with_limits(
        path: impl AsRef<Path>,
        limits: RuntimeLimits,
    ) -> Result<Self, RuntimeError> {
        Self::load_with_engine(path, limits, crate::shared_engine())
    }

    fn load_with_engine(
        path: impl AsRef<Path>,
        limits: RuntimeLimits,
        engine: &Engine,
    ) -> Result<Self, RuntimeError> {
        let path = path.as_ref();
        let component_id = component_identity(path);
        let load_span = info_span!("component.load", component_id = %component_id);
        let bytes = load_span.in_scope(|| read_component(path, &component_id, &limits))?;

        let compile_span = info_span!("component.compile", component_id = %component_id);
        let component = compile_span.in_scope(|| {
            Component::new(engine, &bytes).map_err(|source| {
                RuntimeError::InvalidComponent(
                    ErrorContext::new(
                        "file is not a valid WebAssembly component",
                        &component_id,
                        AppLifecycle::Loaded,
                        None,
                    )
                    .with_source(source),
                )
            })
        })?;

        let instantiate_span = info_span!("component.instantiate", component_id = %component_id);
        instantiate_span.in_scope(|| instantiate(engine, component, component_id, limits))
    }

    #[must_use]
    pub const fn lifecycle(&self) -> AppLifecycle {
        self.lifecycle
    }

    #[must_use]
    pub fn tree(&self) -> Option<&youth_tree::Tree> {
        self.tree.as_ref()
    }

    pub fn mount(&mut self) -> Result<youth_tree::TreeSnapshot, RuntimeError> {
        let turn_id = Some(0);
        let span = info_span!(
            "app.mount",
            component_id = %self.component_id,
            turn_id = 0_u64,
            fuel_before = self.limits.mount.fuel,
            fuel_after = tracing::field::Empty,
            elapsed_microseconds = tracing::field::Empty,
            result = tracing::field::Empty,
        );
        let result = span.in_scope(|| self.mount_inner(turn_id));
        span.record("result", if result.is_ok() { "ok" } else { "error" });
        result
    }

    /// Delivers one host-sequenced activation event and atomically commits its patches.
    pub fn activate(&mut self, node: youth_tree::NodeId) -> Result<TurnReceipt, RuntimeError> {
        if self.lifecycle != AppLifecycle::Mounted {
            return Err(RuntimeError::InvalidLifecycle(self.context(
                format!(
                    "activate is not allowed while the app is {}",
                    self.lifecycle
                ),
                None,
            )));
        }
        let sequence = self
            .last_event_sequence
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                RuntimeError::Internal(self.context("event sequence space is exhausted", None))
            })?;
        self.last_event_sequence = Some(sequence);
        let base_revision = self
            .tree
            .as_ref()
            .expect("mounted applications retain a tree")
            .revision();
        self.last_turn = Some(TurnReceipt {
            turn_id: sequence,
            event_sequence: sequence,
            base_revision,
            next_revision: base_revision,
            patch_count: 0,
            committed: false,
        });
        let span = info_span!(
            "app.turn",
            component_id = %self.component_id,
            turn_id = sequence,
            event_count = 1_u64,
            first_event_sequence = sequence,
            last_event_sequence = sequence,
            base_revision,
            next_revision = tracing::field::Empty,
            patch_count = tracing::field::Empty,
            fuel_before = self.limits.handle.fuel,
            fuel_after = tracing::field::Empty,
            elapsed_microseconds = tracing::field::Empty,
            result = tracing::field::Empty,
        );
        let result = span.in_scope(|| self.activate_inner(node, sequence, base_revision));
        span.record("result", if result.is_ok() { "ok" } else { "error" });
        result
    }

    fn activate_inner(
        &mut self,
        node: youth_tree::NodeId,
        sequence: u64,
        base_revision: u64,
    ) -> Result<TurnReceipt, RuntimeError> {
        let turn_id = Some(sequence);
        self.prepare_call(self.limits.handle, "handle", turn_id)?;
        let events =
            to_guest::event_batch(base_revision, &[HostEvent { sequence, node }], &self.limits)
                .map_err(|message| RuntimeError::Internal(self.context(message, turn_id)))?;

        let started = std::time::Instant::now();
        let guest_result = self
            .bindings
            .youth_app_lifecycle()
            .call_handle(&mut self.store, &events);
        let elapsed = started.elapsed();
        self.record_call_metrics(elapsed);
        let guest_result = match guest_result {
            Ok(result) => result,
            Err(source) => {
                let error = self.classify_guest_failure(source, turn_id, "handling events");
                return Err(self.enter_fault(error));
            }
        };
        let wire_batch = match guest_result {
            Ok(batch) => batch,
            Err(error) => {
                return Err(RuntimeError::GuestRejected(
                    self.guest_error_context(error, turn_id, "handle"),
                ));
            }
        };
        if wire_batch.processed_through != sequence {
            let error = RuntimeError::EventSequenceViolation(self.context(
                format!(
                    "guest processed through event {}, but the last event sent was {sequence}",
                    wire_batch.processed_through
                ),
                turn_id,
            ));
            return Err(self.enter_fault(error));
        }
        let batch = match from_guest::patch_batch(wire_batch, &self.limits) {
            Ok(batch) => batch,
            Err(source) => {
                let category = source.kind;
                let context = self
                    .context("guest returned a malformed patch batch", turn_id)
                    .with_source(source);
                let error = if category == WireErrorKind::TransferLimit {
                    RuntimeError::TransferLimitExceeded(context)
                } else {
                    RuntimeError::InvalidPatchBatch(context)
                };
                return Err(self.enter_fault(error));
            }
        };
        if batch.base_revision != base_revision {
            let error = RuntimeError::RevisionMismatch(self.context(
                format!(
                    "patch base revision {} does not match live revision {base_revision}",
                    batch.base_revision
                ),
                turn_id,
            ));
            return Err(self.enter_fault(error));
        }

        let next_revision = batch.next_revision;
        let patch_count = batch.patches.len();
        if let Some(receipt) = &mut self.last_turn {
            receipt.next_revision = next_revision;
            receipt.patch_count = patch_count;
        }
        tracing::Span::current().record("next_revision", next_revision);
        tracing::Span::current().record("patch_count", patch_count as u64);
        let apply_span = info_span!(
            "tree.apply",
            component_id = %self.component_id,
            turn_id = sequence,
            base_revision,
            next_revision,
            patch_count = patch_count as u64,
        );
        let apply_result = apply_span.in_scope(|| {
            self.tree
                .as_mut()
                .expect("mounted applications retain a tree")
                .apply(batch, &self.limits.tree)
        });
        if let Err(source) = apply_result {
            let revision_error = matches!(
                source,
                youth_tree::PatchError::RevisionMismatch { .. }
                    | youth_tree::PatchError::InvalidRevisionTransition { .. }
            );
            let context = self
                .context("guest returned an invalid patch batch", turn_id)
                .with_source(source);
            let error = if revision_error {
                RuntimeError::RevisionMismatch(context)
            } else {
                RuntimeError::InvalidPatchBatch(context)
            };
            return Err(self.enter_fault(error));
        }

        let receipt = TurnReceipt {
            turn_id: sequence,
            event_sequence: sequence,
            base_revision,
            next_revision,
            patch_count,
            committed: true,
        };
        self.last_turn = Some(receipt.clone());
        Ok(receipt)
    }

    /// Replaces the retained tree with a validated snapshot at the live revision.
    pub fn resync(&mut self) -> Result<youth_tree::TreeSnapshot, RuntimeError> {
        let base_revision = match self.tree.as_ref() {
            Some(tree) if self.lifecycle == AppLifecycle::Mounted => tree.revision(),
            _ => {
                return Err(RuntimeError::InvalidLifecycle(self.context(
                    format!("resync is not allowed while the app is {}", self.lifecycle),
                    None,
                )));
            }
        };
        let turn_id = self.last_event_sequence;
        let span = info_span!(
            "app.resync",
            component_id = %self.component_id,
            turn_id = turn_id.unwrap_or(0),
            event_count = 0_u64,
            first_event_sequence = self.last_event_sequence.unwrap_or(0),
            last_event_sequence = self.last_event_sequence.unwrap_or(0),
            base_revision,
            next_revision = base_revision,
            patch_count = 0_u64,
            fuel_before = self.limits.resync.fuel,
            fuel_after = tracing::field::Empty,
            elapsed_microseconds = tracing::field::Empty,
            result = tracing::field::Empty,
        );
        let result = span.in_scope(|| self.resync_inner(base_revision, turn_id));
        span.record("result", if result.is_ok() { "ok" } else { "error" });
        result
    }

    fn resync_inner(
        &mut self,
        live_revision: u64,
        turn_id: Option<u64>,
    ) -> Result<youth_tree::TreeSnapshot, RuntimeError> {
        self.prepare_call(self.limits.resync, "resync", turn_id)?;
        let started = std::time::Instant::now();
        let guest_result = self
            .bindings
            .youth_app_lifecycle()
            .call_resync(&mut self.store);
        let elapsed = started.elapsed();
        self.record_call_metrics(elapsed);
        let guest_result = match guest_result {
            Ok(result) => result,
            Err(source) => {
                let error = self.classify_guest_failure(source, turn_id, "resynchronizing");
                return Err(self.enter_fault(error));
            }
        };
        let wire_snapshot = match guest_result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Err(RuntimeError::GuestRejected(
                    self.guest_error_context(error, turn_id, "resync"),
                ));
            }
        };
        let snapshot = match from_guest::tree_snapshot(wire_snapshot, &self.limits) {
            Ok(snapshot) => snapshot,
            Err(source) => {
                let category = source.kind;
                let context = self
                    .context("guest returned a malformed resync snapshot", turn_id)
                    .with_source(source);
                let error = if category == WireErrorKind::TransferLimit {
                    RuntimeError::TransferLimitExceeded(context)
                } else {
                    RuntimeError::InvalidSnapshot(context)
                };
                return Err(self.enter_fault(error));
            }
        };
        if snapshot.revision != live_revision {
            let error = RuntimeError::RevisionMismatch(self.context(
                format!(
                    "resync revision {} does not match live revision {live_revision}",
                    snapshot.revision
                ),
                turn_id,
            ));
            return Err(self.enter_fault(error));
        }
        let validation_span = info_span!(
            "tree.validate",
            component_id = %self.component_id,
            turn_id = turn_id.unwrap_or(0),
        );
        let tree = match validation_span
            .in_scope(|| youth_tree::Tree::from_snapshot(snapshot, &self.limits.tree))
        {
            Ok(tree) => tree,
            Err(source) => {
                let error = RuntimeError::InvalidSnapshot(
                    self.context("guest returned an invalid resync tree", turn_id)
                        .with_source(source),
                );
                return Err(self.enter_fault(error));
            }
        };
        let snapshot = tree.to_snapshot();
        self.tree = Some(tree);
        Ok(snapshot)
    }

    /// Stops a mounted application and releases its retained tree.
    pub fn stop(&mut self) -> Result<(), RuntimeError> {
        if self.lifecycle != AppLifecycle::Mounted {
            return Err(RuntimeError::InvalidLifecycle(self.context(
                format!("stop is not allowed while the app is {}", self.lifecycle),
                None,
            )));
        }
        self.tree = None;
        self.lifecycle = AppLifecycle::Stopped;
        Ok(())
    }

    /// Returns host-owned state without entering the guest.
    #[must_use]
    pub fn inspect(&self) -> AppInspection {
        let tree = self.tree.as_ref();
        AppInspection {
            lifecycle: self.lifecycle,
            world: APPLICATION_WORLD.to_owned(),
            current_revision: tree.map(youth_tree::Tree::revision),
            next_event_sequence: self.last_event_sequence.unwrap_or(0).checked_add(1),
            last_event_sequence: self.last_event_sequence,
            node_count: tree.map_or(0, youth_tree::Tree::node_count),
            depth: tree.map_or(0, youth_tree::Tree::depth),
            last_turn: self.last_turn.clone(),
            fault: self.fault.clone(),
            canonical_tree: tree.map_or_else(String::new, youth_tree::Tree::canonical),
        }
    }

    fn mount_inner(
        &mut self,
        turn_id: Option<u64>,
    ) -> Result<youth_tree::TreeSnapshot, RuntimeError> {
        if self.lifecycle != AppLifecycle::Loaded {
            return Err(RuntimeError::InvalidLifecycle(self.context(
                format!("mount is not allowed while the app is {}", self.lifecycle),
                turn_id,
            )));
        }
        self.prepare_call(self.limits.mount, "mount", turn_id)?;

        let started = std::time::Instant::now();
        let guest_result = self
            .bindings
            .youth_app_lifecycle()
            .call_mount(&mut self.store);
        let elapsed = started.elapsed();
        self.record_call_metrics(elapsed);
        let guest_result = match guest_result {
            Ok(result) => result,
            Err(source) => {
                let error = self.classify_guest_failure(source, turn_id, "mounting");
                return Err(self.enter_fault(error));
            }
        };
        let wire_snapshot = match guest_result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Err(RuntimeError::GuestRejected(
                    self.guest_error_context(error, turn_id, "mount"),
                ));
            }
        };
        let snapshot = match from_guest::tree_snapshot(wire_snapshot, &self.limits) {
            Ok(snapshot) => snapshot,
            Err(source) => {
                let category = source.kind;
                let context = self
                    .context("guest returned a malformed initial snapshot", turn_id)
                    .with_source(source);
                let error = if category == WireErrorKind::TransferLimit {
                    RuntimeError::TransferLimitExceeded(context)
                } else {
                    RuntimeError::InvalidSnapshot(context)
                };
                return Err(self.enter_fault(error));
            }
        };
        if snapshot.revision != 0 {
            let error = RuntimeError::RevisionMismatch(self.context(
                format!(
                    "initial snapshot revision must be 0, got {}",
                    snapshot.revision
                ),
                turn_id,
            ));
            return Err(self.enter_fault(error));
        }

        let validation_span =
            info_span!("tree.validate", component_id = %self.component_id, turn_id = 0_u64);
        let tree = validation_span
            .in_scope(|| youth_tree::Tree::from_snapshot(snapshot, &self.limits.tree))
            .map_err(|source| {
                RuntimeError::InvalidSnapshot(
                    self.context("guest returned an invalid initial tree", turn_id)
                        .with_source(source),
                )
            });
        let tree = match tree {
            Ok(tree) => tree,
            Err(error) => return Err(self.enter_fault(error)),
        };
        let snapshot = tree.to_snapshot();
        self.tree = Some(tree);
        self.lifecycle = AppLifecycle::Mounted;
        Ok(snapshot)
    }

    fn guest_error_context(
        &self,
        error: crate::bindings::youth::app::ui::AppError,
        turn_id: Option<u64>,
        operation: &str,
    ) -> ErrorContext {
        let code = format!("{:?}", error.code);
        let message = error.message.map_or_else(
            || format!("guest rejected {operation} with {code}"),
            |message| {
                if message.len() > self.limits.max_guest_error_message {
                    format!("guest rejected {operation} with {code}; error message exceeded the configured limit")
                } else {
                    format!("guest rejected {operation} with {code}: {message}")
                }
            },
        );
        self.context(message, turn_id)
    }

    fn prepare_call(
        &mut self,
        budget: CallBudget,
        operation: &str,
        turn_id: Option<u64>,
    ) -> Result<(), RuntimeError> {
        self.store.set_fuel(budget.fuel).map_err(|source| {
            RuntimeError::Internal(
                self.context(format!("failed to set {operation} fuel"), turn_id)
                    .with_source(source),
            )
        })?;
        self.store
            .set_hostcall_fuel(self.limits.max_guest_to_host_transfer);
        self.store
            .set_epoch_deadline(deadline_ticks(budget.deadline));
        self.store.epoch_deadline_trap();
        self.store.data_mut().limiter.limit_hit = false;
        Ok(())
    }

    fn record_call_metrics(&self, elapsed: std::time::Duration) {
        tracing::Span::current().record("elapsed_microseconds", elapsed.as_micros() as u64);
        if let Ok(fuel_after) = self.store.get_fuel() {
            tracing::Span::current().record("fuel_after", fuel_after);
        }
    }

    fn classify_guest_failure(
        &self,
        source: wasmtime::Error,
        turn_id: Option<u64>,
        operation: &str,
    ) -> RuntimeError {
        let trap = source.downcast_ref::<wasmtime::Trap>().copied();
        let hostcall_fuel_exhausted = source
            .root_cause()
            .to_string()
            .contains("fuel allocated for hostcalls has been exhausted");
        let context = self
            .context(format!("guest trapped while {operation}"), turn_id)
            .with_source(source);
        if self.store.data().limiter.limit_hit {
            RuntimeError::MemoryLimitExceeded(context)
        } else if hostcall_fuel_exhausted {
            RuntimeError::TransferLimitExceeded(context)
        } else if trap == Some(wasmtime::Trap::OutOfFuel) {
            RuntimeError::FuelExhausted(context)
        } else if trap == Some(wasmtime::Trap::Interrupt) {
            RuntimeError::DeadlineExceeded(context)
        } else {
            RuntimeError::GuestTrap(context)
        }
    }

    fn enter_fault(&mut self, error: RuntimeError) -> RuntimeError {
        self.fault = Some(AppFault {
            category: error.category(),
            message: error.context().message.clone(),
        });
        self.lifecycle = AppLifecycle::Faulted;
        let span = info_span!(
            "app.fault",
            component_id = %self.component_id,
            category = ?error.category(),
            turn_id = error.context().turn_id.unwrap_or(0),
        );
        span.in_scope(|| {});
        error
    }

    fn context(&self, message: impl Into<String>, turn_id: Option<u64>) -> ErrorContext {
        ErrorContext::new(message, &self.component_id, self.lifecycle, turn_id)
    }
}

fn instantiate(
    engine: &Engine,
    component: Component,
    component_id: String,
    limits: RuntimeLimits,
) -> Result<YouthApp, RuntimeError> {
    let mut linker = Linker::<HostState>::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|source| {
        RuntimeError::LinkFailure(
            ErrorContext::new(
                "failed to configure closed WASIp2 imports",
                &component_id,
                AppLifecycle::Loaded,
                None,
            )
            .with_source(source),
        )
    })?;
    let pre = linker.instantiate_pre(&component).map_err(|source| {
        RuntimeError::LinkFailure(
            ErrorContext::new(
                "component imports are incompatible with the Youth host",
                &component_id,
                AppLifecycle::Loaded,
                None,
            )
            .with_source(source),
        )
    })?;
    let pre = ApplicationPre::new(pre).map_err(|source| {
        RuntimeError::UnsupportedWorld(
            ErrorContext::new(
                "component does not export youth:app/application@0.0.1",
                &component_id,
                AppLifecycle::Loaded,
                None,
            )
            .with_source(source),
        )
    })?;

    let state = HostState {
        table: ResourceTable::new(),
        wasi: WasiCtxBuilder::new().build(),
        limiter: MemoryLimiter {
            maximum: limits.max_linear_memory,
            max_table_elements: limits.max_table_elements,
            limit_hit: false,
        },
    };
    let mut store = Store::new(engine, state);
    store.limiter(|state| &mut state.limiter);
    // Instantiation runs guest start code, so it is budgeted like any
    // other guest call. A sentinel deadline is not usable here:
    // Wasmtime adds the delta to the current epoch and would overflow.
    store.set_epoch_deadline(deadline_ticks(limits.mount.deadline));
    store.epoch_deadline_trap();
    store.set_hostcall_fuel(limits.max_guest_to_host_transfer);
    store.set_fuel(limits.mount.fuel).map_err(|source| {
        RuntimeError::Internal(
            ErrorContext::new(
                "failed to budget fuel for instantiation",
                &component_id,
                AppLifecycle::Loaded,
                None,
            )
            .with_source(source),
        )
    })?;
    let bindings = pre.instantiate(&mut store).map_err(|source| {
        let context = ErrorContext::new(
            "component instantiation failed",
            &component_id,
            AppLifecycle::Loaded,
            None,
        )
        .with_source(source);
        if store.data().limiter.limit_hit {
            RuntimeError::MemoryLimitExceeded(context)
        } else {
            RuntimeError::InstantiationFailure(context)
        }
    })?;
    Ok(YouthApp {
        component_id,
        limits,
        store,
        bindings,
        tree: None,
        lifecycle: AppLifecycle::Loaded,
        last_event_sequence: None,
        last_turn: None,
        fault: None,
    })
}

/// Lists the interfaces a component imports, sorted and deduplicated.
///
/// Youth's WASI context is closed, so these imports are inert: the host
/// decides what they return. They are still a real linking and
/// compatibility surface, so the import list is treated as a dependency
/// budget rather than left implicit (see `docs/GUEST-PROFILE.md`).
///
/// Names are returned without their `@version` suffix so that a patch
/// bump in the WASI snapshot does not read as a new capability.
pub fn component_imports(path: impl AsRef<Path>) -> Result<Vec<String>, RuntimeError> {
    let path = path.as_ref();
    let component_id = component_identity(path);
    let limits = RuntimeLimits::default();
    let engine = crate::shared_engine();
    let bytes = read_component(path, &component_id, &limits)?;
    let component = Component::new(engine, &bytes).map_err(|source| {
        RuntimeError::InvalidComponent(
            ErrorContext::new(
                "file is not a valid WebAssembly component",
                &component_id,
                AppLifecycle::Loaded,
                None,
            )
            .with_source(source),
        )
    })?;
    let mut imports: Vec<String> = component
        .component_type()
        .imports(engine)
        .map(|(name, _)| {
            name.split_once('@')
                .map_or(name, |(base, _)| base)
                .to_owned()
        })
        .collect();
    imports.sort_unstable();
    imports.dedup();
    Ok(imports)
}

fn read_component(
    path: &Path,
    component_id: &str,
    limits: &RuntimeLimits,
) -> Result<Vec<u8>, RuntimeError> {
    let file = File::open(path).map_err(|source| {
        RuntimeError::InvalidComponent(
            ErrorContext::new(
                "component file could not be opened",
                component_id,
                AppLifecycle::Loaded,
                None,
            )
            .with_source(source),
        )
    })?;
    let metadata = file.metadata().map_err(|source| {
        RuntimeError::InvalidComponent(
            ErrorContext::new(
                "component file metadata could not be read",
                component_id,
                AppLifecycle::Loaded,
                None,
            )
            .with_source(source),
        )
    })?;
    if metadata.len() > limits.max_component_size as u64 {
        return Err(RuntimeError::ComponentTooLarge(ErrorContext::new(
            format!(
                "component is {} bytes, exceeding the {}-byte limit",
                metadata.len(),
                limits.max_component_size
            ),
            component_id,
            AppLifecycle::Loaded,
            None,
        )));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(limits.max_component_size as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| {
            RuntimeError::InvalidComponent(
                ErrorContext::new(
                    "component file could not be read",
                    component_id,
                    AppLifecycle::Loaded,
                    None,
                )
                .with_source(source),
            )
        })?;
    if bytes.len() > limits.max_component_size {
        return Err(RuntimeError::ComponentTooLarge(ErrorContext::new(
            format!(
                "component exceeds the {}-byte limit",
                limits.max_component_size
            ),
            component_id,
            AppLifecycle::Loaded,
            None,
        )));
    }
    Ok(bytes)
}

fn component_identity(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| PathBuf::from(path).display().to_string(), String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_growth_is_bounded_and_recorded_as_a_resource_limit() {
        let mut limiter = MemoryLimiter {
            maximum: 1024,
            max_table_elements: 4,
            limit_hit: false,
        };

        assert!(limiter.table_growing(0, 4, None).unwrap());
        assert!(!limiter.table_growing(4, 5, None).unwrap());
        assert!(limiter.limit_hit);
    }
}
