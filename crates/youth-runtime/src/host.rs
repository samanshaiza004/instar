use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use tracing::{Instrument, info_span};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, ResourceLimiter, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bindings::{Application, ApplicationPre};
use crate::engine::deadline_ticks;
use crate::error::ErrorContext;
use crate::wire::from_guest::{self, WireErrorKind};
use crate::{RuntimeError, RuntimeLimits};

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

struct MemoryLimiter {
    maximum: usize,
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
        _desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(true)
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
        self.store
            .set_fuel(self.limits.mount.fuel)
            .map_err(|source| {
                RuntimeError::Internal(
                    self.context("failed to set mount fuel", turn_id)
                        .with_source(source),
                )
            })?;
        self.store
            .set_epoch_deadline(deadline_ticks(self.limits.mount.deadline));
        self.store.epoch_deadline_trap();
        self.store.data_mut().limiter.limit_hit = false;

        let started = std::time::Instant::now();
        let guest_result = self
            .bindings
            .youth_app_lifecycle()
            .call_mount(&mut self.store);
        let elapsed = started.elapsed();
        tracing::Span::current().record("elapsed_microseconds", elapsed.as_micros() as u64);
        if let Ok(fuel_after) = self.store.get_fuel() {
            tracing::Span::current().record("fuel_after", fuel_after);
        }
        let guest_result = match guest_result {
            Ok(result) => result,
            Err(source) => {
                self.lifecycle = AppLifecycle::Faulted;
                return Err(self.classify_guest_failure(source, elapsed, turn_id));
            }
        };
        let wire_snapshot = match guest_result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Err(RuntimeError::GuestRejected(
                    self.guest_error_context(error, turn_id),
                ));
            }
        };
        let snapshot = match from_guest::tree_snapshot(wire_snapshot, &self.limits) {
            Ok(snapshot) => snapshot,
            Err(source) => {
                self.lifecycle = AppLifecycle::Faulted;
                let context = self
                    .context("guest returned a malformed initial snapshot", turn_id)
                    .with_source(source);
                return Err(
                    if context.source_error().is_some_and(|source| {
                        source
                            .downcast_ref::<from_guest::WireError>()
                            .is_some_and(|error| error.kind == WireErrorKind::TransferLimit)
                    }) {
                        RuntimeError::TransferLimitExceeded(context)
                    } else {
                        RuntimeError::InvalidSnapshot(context)
                    },
                );
            }
        };
        if snapshot.revision != 0 {
            self.lifecycle = AppLifecycle::Faulted;
            return Err(RuntimeError::RevisionMismatch(self.context(
                format!(
                    "initial snapshot revision must be 0, got {}",
                    snapshot.revision
                ),
                turn_id,
            )));
        }

        let validation_span =
            info_span!("tree.validate", component_id = %self.component_id, turn_id = 0_u64);
        let tree = youth_tree::Tree::from_snapshot(snapshot, &self.limits.tree)
            .instrument(validation_span)
            .into_inner()
            .map_err(|source| {
                self.lifecycle = AppLifecycle::Faulted;
                RuntimeError::InvalidSnapshot(
                    self.context("guest returned an invalid initial tree", turn_id)
                        .with_source(source),
                )
            })?;
        let snapshot = tree.to_snapshot();
        self.tree = Some(tree);
        self.lifecycle = AppLifecycle::Mounted;
        Ok(snapshot)
    }

    fn guest_error_context(
        &self,
        error: crate::bindings::youth::app::ui::AppError,
        turn_id: Option<u64>,
    ) -> ErrorContext {
        let code = format!("{:?}", error.code);
        let message = error.message.map_or_else(
            || format!("guest rejected mount with {code}"),
            |message| {
                if message.len() > self.limits.max_guest_error_message {
                    format!("guest rejected mount with {code}; error message exceeded the configured limit")
                } else {
                    format!("guest rejected mount with {code}: {message}")
                }
            },
        );
        self.context(message, turn_id)
    }

    fn classify_guest_failure(
        &self,
        source: wasmtime::Error,
        elapsed: std::time::Duration,
        turn_id: Option<u64>,
    ) -> RuntimeError {
        let context = self
            .context("guest trapped while mounting", turn_id)
            .with_source(source);
        if self.store.data().limiter.limit_hit {
            RuntimeError::MemoryLimitExceeded(context)
        } else if self.store.get_fuel().is_ok_and(|fuel| fuel == 0) {
            RuntimeError::FuelExhausted(context)
        } else if elapsed >= self.limits.mount.deadline {
            RuntimeError::DeadlineExceeded(context)
        } else {
            RuntimeError::GuestTrap(context)
        }
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
    })
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
