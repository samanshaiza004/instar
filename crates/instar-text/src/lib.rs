//! Instar's text resource subsystem.
//!
//! # What this crate is for
//!
//! Phase 2 proved a Wasm guest can describe a desktop interface while the host
//! owns presentation and every piece of transient interaction. But every piece
//! of state the host owned there was also state the host *invented*: focus,
//! scroll offsets, pressed state and hover have no guest-side counterpart to
//! disagree with.
//!
//! A document does. It is genuinely the guest's, and typing is the most
//! latency-sensitive interaction there is — so the guest cannot sit between a
//! keystroke and a glyph. The host therefore keeps a replica it edits
//! immediately, and the guest holds the canonical document and hears about
//! edits afterwards. That is a synchronization problem, and pretending
//! otherwise is how it becomes a correctness problem.
//!
//! # Why this is not part of `instar-ui`
//!
//! The criterion is not "who owns it" — the host owns both — but:
//!
//! > Is this state's lifetime and meaning subordinate to the semantic UI tree?
//!
//! Yes for `FocusState` and `ScrollState`: a `Scroll` *is* a tree node, focus
//! points at a `NodeKey`, and both obey the retirement rule that state naming a
//! key dies when the key stops being eligible.
//!
//! No for text. A buffer must outlive any snapshot that happens to show it; two
//! views over one buffer has no tree analogue; and a guest commit that removes
//! an editor node removes a *presentation*, which must not be able to destroy a
//! document. So this is a sibling of `instar-ui`, composed by `instar-host`,
//! and `instar-ui` does not depend on it — a layering test holds that edge
//! absent rather than merely unused.
//!
//! # Coordinates
//!
//! > Instar text edits use UTF-8 byte ranges. Consumers requiring additional
//! > coordinate representations derive them from their own text state.
//!
//! Byte ranges because that is what the storage edits in and what the revision
//! protocol carries — not because downstream needs nothing else. Tree-sitter's
//! `InputEdit` wants three byte offsets *and* three row-column points, and the
//! guest owns the document it would derive those from.
//!
//! # The invariant
//!
//! > No latency-sensitive operation may require materializing the entire
//! > `TextBuffer` contiguously.
//!
//! A rope makes that avoidable; it does not make it automatic. `textbench`
//! measures whether anything takes the offer.
//!
//! # Package A's scope
//!
//! Local editing only. No synchronization state lives here yet: `session_epoch`,
//! `guest_ack_revision` and a pending queue belong to a synchronization
//! *session*, not to a buffer, and in a host-local package a pending queue could
//! never drain — every memory measurement would carry an unbounded backlog for a
//! subsystem that is not being tested. Edits return [`AppliedEdit`], which is
//! what that later package consumes.

#![forbid(unsafe_code)]

mod buffer;
mod edit;
pub mod instrument;
mod storage;
mod system;
mod view;
mod viewport;

pub use buffer::TextBuffer;
pub use edit::{AppliedEdit, EditJournal, TextEdit};
pub use storage::{TextSlice, TextStorage};
pub use system::{MAX_TEXT_BUFFERS, MAX_TEXT_VIEWS, TextSystem};
pub use view::{Selection, TextView};
pub use viewport::{MAX_SHAPED_PARAGRAPH_BYTES, ParagraphWindow, ShapingWindow, TextViewport};

/// A buffer's version, advancing once per applied edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Revision(pub u64);

impl Revision {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl std::fmt::Display for Revision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "r{}", self.0)
    }
}

/// A handle to a buffer.
///
/// Generational from the first line of this crate, deliberately. Instar has
/// already paid for this lesson once: `NodeKey` needed a generation because an
/// id reused after retirement lets a queued event reach the node that replaced
/// it. These handles will cross the guest boundary in the synchronization
/// package, where the same hole would be a protocol break to close rather than
/// a field to add.
///
/// ```text
/// guest holds TextViewId(12)  ->  view 12 dropped  ->  12 reused
///                             ->  a stale request mutates the new view
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TextBufferId {
    pub id: u32,
    pub generation: u32,
}

/// A handle to a view. Generational for the same reason as [`TextBufferId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TextViewId {
    pub id: u32,
    pub generation: u32,
}

impl std::fmt::Display for TextBufferId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "buffer{}#{}", self.id, self.generation)
    }
}

impl std::fmt::Display for TextViewId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "view{}#{}", self.id, self.generation)
    }
}

/// Everything this crate refuses to do, and why.
///
/// Every variant is a case `crop` would panic on, a handle that named
/// something no longer there, or an edit that cannot be expressed. All of them
/// leave the subsystem exactly as it was — a refused edit is not a partial one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    #[error("byte range {start}..{end} is inverted")]
    InvertedRange { start: usize, end: usize },
    #[error("byte range {start}..{end} is out of bounds for a {len}-byte buffer")]
    RangeOutOfBounds {
        start: usize,
        end: usize,
        len: usize,
    },
    #[error("byte {byte} is not a UTF-8 character boundary")]
    NotACharBoundary { byte: usize },
    #[error("line {line} is out of bounds for a {lines}-line buffer")]
    LineOutOfBounds { line: usize, lines: usize },
    /// A handle that named a buffer this system does not have, or names one it
    /// no longer has. The generation is what distinguishes the two.
    #[error("{0} is not a live buffer")]
    NoSuchBuffer(TextBufferId),
    #[error("{0} is not a live view")]
    NoSuchView(TextViewId),
    /// Live buffers hit their ceiling. A bound on *live* resources, not on
    /// historical identities: slots are reused and generations advance per
    /// slot, so there is no second ledger growing with time.
    #[error("{limit} buffers are already live")]
    TooManyBuffers { limit: usize },
    #[error("{limit} views are already live")]
    TooManyViews { limit: usize },
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("nothing to redo")]
    NothingToRedo,
}
