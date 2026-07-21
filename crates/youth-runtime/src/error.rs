use std::error::Error;
use std::fmt;

use crate::AppLifecycle;

/// Stable, machine-readable runtime error categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeErrorCategory {
    ComponentTooLarge,
    InvalidComponent,
    UnsupportedWorld,
    LinkFailure,
    InstantiationFailure,
    InvalidLifecycle,
    GuestRejected,
    GuestTrap,
    FuelExhausted,
    DeadlineExceeded,
    MemoryLimitExceeded,
    TransferLimitExceeded,
    InvalidSnapshot,
    InvalidPatchBatch,
    RevisionMismatch,
    EventSequenceViolation,
    StateUnavailable,
    StateCommitFailed,
    WorkerStopped,
    Internal,
}

/// Diagnostic context attached to every stable error category.
#[derive(Debug)]
pub struct ErrorContext {
    pub message: String,
    pub component_id: String,
    pub lifecycle: AppLifecycle,
    pub turn_id: Option<u64>,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ErrorContext {
    pub(crate) fn new(
        message: impl Into<String>,
        component_id: impl Into<String>,
        lifecycle: AppLifecycle,
        turn_id: Option<u64>,
    ) -> Self {
        Self {
            message: message.into(),
            component_id: component_id.into(),
            lifecycle,
            turn_id,
            source: None,
        }
    }

    /// Attaches an underlying cause.
    ///
    /// The bound is `Into<Box<dyn Error>>` rather than `Error` so that
    /// `wasmtime::Error` (an `anyhow::Error`, which does not implement
    /// `std::error::Error`) can be carried here. Wasmtime's message stays
    /// in this source chain and never becomes Youth's stable API surface.
    pub(crate) fn with_source(
        mut self,
        source: impl Into<Box<dyn Error + Send + Sync + 'static>>,
    ) -> Self {
        self.source = Some(source.into());
        self
    }

    #[must_use]
    pub fn source_error(&self) -> Option<&(dyn Error + Send + Sync + 'static)> {
        self.source.as_deref()
    }
}

impl fmt::Display for ErrorContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} (component: {}, lifecycle: {}",
            self.message, self.component_id, self.lifecycle
        )?;
        if let Some(turn_id) = self.turn_id {
            write!(formatter, ", turn: {turn_id}")?;
        }
        formatter.write_str(")")
    }
}

macro_rules! runtime_errors {
    ($($variant:ident),+ $(,)?) => {
        /// A runtime failure with a stable category and contextual diagnostics.
        #[derive(Debug, thiserror::Error)]
        pub enum RuntimeError {
            $(
                #[error("{0}")]
                $variant(ErrorContext),
            )+
        }

        impl RuntimeError {
            #[must_use]
            pub const fn category(&self) -> RuntimeErrorCategory {
                match self {
                    $(Self::$variant(_) => RuntimeErrorCategory::$variant,)+
                }
            }

            #[must_use]
            pub const fn context(&self) -> &ErrorContext {
                match self {
                    $(Self::$variant(context) => context,)+
                }
            }
        }
    };
}

runtime_errors!(
    ComponentTooLarge,
    InvalidComponent,
    UnsupportedWorld,
    LinkFailure,
    InstantiationFailure,
    InvalidLifecycle,
    GuestRejected,
    GuestTrap,
    FuelExhausted,
    DeadlineExceeded,
    MemoryLimitExceeded,
    TransferLimitExceeded,
    InvalidSnapshot,
    InvalidPatchBatch,
    RevisionMismatch,
    EventSequenceViolation,
    StateUnavailable,
    StateCommitFailed,
    WorkerStopped,
    Internal,
);
