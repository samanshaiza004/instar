//! A policy-agnostic recovery harness.
//!
//! This crate is not a production Instar feature and is not wired into
//! `instar-host`. The document/architecture audit that preceded it found
//! that no host-side recovery mechanism for guest text -- no journal, no
//! checkpoint, no durability of any kind -- currently exists, was ever
//! shipped, or was ever precisely promised in writing anywhere in this
//! project's history. This crate does not decide what such a mechanism
//! should guarantee. It builds the smallest real journal-and-checkpoint
//! implementation capable of being asked, and a harness that asks it, so
//! that whichever [`RecoveryPolicy`] gets chosen later can be proven against
//! real failure injection -- including real process death, not a dropped
//! in-process object standing in for one -- rather than asserted.
//!
//! # What is, and is not, configurable
//!
//! [`RecoveryPolicy`] is the *only* thing a test or a future decision-maker
//! chooses. Everything downstream of it -- the journal record format, the
//! checksum, the replay order, the checkpoint-then-prune sequencing at a
//! generation handoff -- is fixed, because those are correctness properties
//! of the mechanism, not policy knobs. A policy can choose *whether* to
//! journal every edit and *whether* to fsync; it cannot choose whether a
//! torn record is detected, because "sometimes" is not an answer to that
//! question.

pub mod checkpoint;
pub mod checksum;
pub mod journal;
pub mod recovery;

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};

pub use journal::TailFault;
pub use recovery::{RecoveredState, recover};

/// Whether, and how durably, host-local edits are journaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalMode {
    /// No journal. An edit exists only in memory until the next checkpoint,
    /// or forever if checkpoints are also disabled -- this is the current,
    /// real Instar posture for guest text, restated as a policy value
    /// rather than left implicit.
    Disabled,
    /// Every edit is appended before `apply_edit` returns. `fsync` decides
    /// whether that append is durable (survives a real process kill) or
    /// merely written (survives a clean restart, via the OS page cache, but
    /// makes no promise about a real crash).
    AppendEveryEdit { fsync: bool },
}

/// When a full-document checkpoint is written, pruning the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointTrigger {
    Disabled,
    EveryNEdits(u32),
}

/// What happens to an existing checkpoint when `InputOverflow` tears down a
/// generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowAction {
    /// The checkpoint is exactly as valid for the replacement generation as
    /// it was for the one that just got torn down -- an input-queue bound
    /// says nothing about whether the document itself is trustworthy.
    PreserveCheckpoint,
    DeleteCheckpoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryPolicy {
    pub journal: JournalMode,
    pub checkpoint: CheckpointTrigger,
    pub overflow_action: OverflowAction,
    pub input_queue_capacity: usize,
}

impl RecoveryPolicy {
    /// The current, real posture: nothing is durable, nothing is
    /// recoverable. Exercising this through the same harness as every other
    /// policy is itself a claim worth proving -- that a policy which
    /// promises nothing loses nothing beyond what it already disclaimed.
    pub const NONE: Self = Self {
        journal: JournalMode::Disabled,
        checkpoint: CheckpointTrigger::Disabled,
        overflow_action: OverflowAction::DeleteCheckpoint,
        input_queue_capacity: 8,
    };

    /// Every edit fsynced before it is visible. The strongest policy this
    /// harness can express: a real process kill loses at most the edit that
    /// was mid-write when it died, never one already acknowledged.
    pub const JOURNAL_EVERY_EDIT_DURABLE: Self = Self {
        journal: JournalMode::AppendEveryEdit { fsync: true },
        checkpoint: CheckpointTrigger::EveryNEdits(4),
        overflow_action: OverflowAction::PreserveCheckpoint,
        input_queue_capacity: 8,
    };

    /// Every edit journaled, but only `write`, never `fsync`'d until the
    /// next checkpoint. Survives a clean restart; a real crash can lose
    /// whatever the OS had not yet flushed from its page cache.
    pub const JOURNAL_EVERY_EDIT_BUFFERED: Self = Self {
        journal: JournalMode::AppendEveryEdit { fsync: false },
        checkpoint: CheckpointTrigger::EveryNEdits(4),
        overflow_action: OverflowAction::PreserveCheckpoint,
        input_queue_capacity: 8,
    };

    /// No journal at all -- only periodic full checkpoints. Recovery can
    /// lose up to a whole checkpoint interval's worth of edits, never more.
    pub const CHECKPOINT_ONLY: Self = Self {
        journal: JournalMode::Disabled,
        checkpoint: CheckpointTrigger::EveryNEdits(1),
        overflow_action: OverflowAction::PreserveCheckpoint,
        input_queue_capacity: 8,
    };

    /// Looks up one of the named policies above by a stable string name,
    /// shared between the test harness (in-process) and `fault_child` (a
    /// separate binary, which cannot share a Rust value across the process
    /// boundary and so is handed a name on its command line instead).
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "none" => Some(Self::NONE),
            "durable" => Some(Self::JOURNAL_EVERY_EDIT_DURABLE),
            "buffered" => Some(Self::JOURNAL_EVERY_EDIT_BUFFERED),
            "checkpoint-only" => Some(Self::CHECKPOINT_ONLY),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GenerationId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEvent(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputOverflow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditOutcome {
    pub sequence: u64,
    /// Whether this specific edit was fsynced before `apply_edit` returned.
    /// `false` under a buffered or disabled journal does not mean the edit
    /// is lost -- only that this call makes no promise about a real crash.
    pub durable: bool,
}

/// The host side of one document: exactly the state a real Instar host
/// process would hold in memory, plus the on-disk journal and checkpoint a
/// chosen [`RecoveryPolicy`] might add.
pub struct Host {
    dir: PathBuf,
    policy: RecoveryPolicy,
    journal: journal::JournalWriter,
    document: String,
    sequence: u64,
    edits_since_checkpoint: u32,
    generation: GenerationId,
    input_queue: VecDeque<InputEvent>,
    /// Set by a test to represent "the guest has incorporated this edit
    /// into its own document," entirely separate from anything durable.
    /// Recovery must never depend on this value -- proving that is the
    /// point of the guest-trap scenarios.
    guest_consumed_sequence: Option<u64>,
}

impl Host {
    pub fn open(dir: impl Into<PathBuf>, policy: RecoveryPolicy) -> io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let journal = journal::JournalWriter::open(&Self::journal_path_for(&dir))?;
        Ok(Self {
            dir,
            policy,
            journal,
            document: String::new(),
            sequence: 0,
            edits_since_checkpoint: 0,
            generation: GenerationId(1),
            input_queue: VecDeque::new(),
            guest_consumed_sequence: None,
        })
    }

    /// Where a `Host` opened against `dir` keeps its journal. Public and
    /// callable without a live `Host`, so a test can compute it after the
    /// `Host` that wrote it is gone -- standing in for a fresh process (or
    /// a fresh generation) that never had one.
    pub fn journal_path_for(dir: &Path) -> PathBuf {
        dir.join("journal.bin")
    }

    pub fn checkpoint_path_for(dir: &Path) -> PathBuf {
        dir.join("checkpoint.bin")
    }

    pub fn journal_path(&self) -> PathBuf {
        Self::journal_path_for(&self.dir)
    }

    pub fn checkpoint_path(&self) -> PathBuf {
        Self::checkpoint_path_for(&self.dir)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn policy(&self) -> RecoveryPolicy {
        self.policy
    }

    pub fn generation(&self) -> GenerationId {
        self.generation
    }

    /// The document as this live process currently has it -- what would be
    /// on screen. Distinct from what `recover()` would reconstruct from
    /// disk, which may lag it under any policy weaker than
    /// `JOURNAL_EVERY_EDIT_DURABLE`.
    pub fn document(&self) -> &str {
        &self.document
    }

    /// "Last presented sequence": the newest edit this live process has
    /// applied to its own in-memory document, independent of what is
    /// durable. Captured by a test *before* injecting a failure, then
    /// compared against `RecoveredState::last_recovered_sequence` after, to
    /// state precisely how much a given policy allowed to be lost.
    pub fn last_presented_sequence(&self) -> u64 {
        self.sequence
    }

    pub fn mark_guest_consumed(&mut self, sequence: u64) {
        self.guest_consumed_sequence = Some(sequence);
    }

    pub fn guest_consumed_sequence(&self) -> Option<u64> {
        self.guest_consumed_sequence
    }

    /// Test-only access to the journal writer, for fault-injecting the next
    /// append in-process -- proving what a real crash mid-write would leave
    /// behind without needing a subprocess for every such case. Real
    /// process death is proven separately, with a real subprocess.
    pub fn journal_writer_mut(&mut self) -> &mut journal::JournalWriter {
        &mut self.journal
    }

    pub fn journal_size_bytes(&self) -> io::Result<u64> {
        self.journal.len_bytes()
    }

    /// The one edit path. A journal write that returns `Err` leaves `self`
    /// completely unchanged -- nothing below the write may run until it has
    /// returned `Ok`, which is the entire content of what "mark edit
    /// durable before write completes" would violate.
    pub fn apply_edit(&mut self, delta: impl Into<String>) -> io::Result<EditOutcome> {
        let delta = delta.into();
        let candidate_sequence = self.sequence + 1;

        let durable = if let JournalMode::AppendEveryEdit { fsync } = self.policy.journal {
            self.journal
                .append(candidate_sequence, delta.as_bytes(), fsync)?;
            fsync
        } else {
            false
        };

        self.sequence = candidate_sequence;
        self.document.push_str(&delta);
        self.edits_since_checkpoint += 1;

        if let CheckpointTrigger::EveryNEdits(n) = self.policy.checkpoint
            && self.edits_since_checkpoint >= n
        {
            self.checkpoint()?;
        }

        Ok(EditOutcome {
            sequence: candidate_sequence,
            durable,
        })
    }

    /// Writes a full-document checkpoint at the current sequence, then
    /// prunes the journal: everything the journal held is now captured by
    /// the checkpoint, so keeping it too would only make recovery data grow
    /// without bound over the life of a document.
    pub fn checkpoint(&mut self) -> io::Result<()> {
        let checkpoint = checkpoint::Checkpoint {
            sequence: self.sequence,
            content: self.document.as_bytes().to_vec(),
        };
        checkpoint::write_checkpoint(
            &self.checkpoint_path(),
            &checkpoint,
            checkpoint::WriteFault::None,
        )?;
        self.journal.truncate()?;
        self.edits_since_checkpoint = 0;
        Ok(())
    }

    /// Pushes one input event, or reports overflow and tears down the
    /// current generation. Matches the real Surface protocol's policy
    /// (`docs/PHASE-3.md`): an event that cannot enter the bounded queue
    /// terminates the generation outright rather than degrading into a
    /// lossy notification.
    pub fn push_input(&mut self, event: InputEvent) -> Result<(), InputOverflow> {
        if self.input_queue.len() >= self.policy.input_queue_capacity {
            self.handle_overflow();
            return Err(InputOverflow);
        }
        self.input_queue.push_back(event);
        Ok(())
    }

    pub fn input_queue_len(&self) -> usize {
        self.input_queue.len()
    }

    fn handle_overflow(&mut self) {
        if self.policy.overflow_action == OverflowAction::DeleteCheckpoint {
            let _ = checkpoint::delete(&self.checkpoint_path());
        }
        self.replace_generation();
    }

    /// Replaces the generation with no handoff: the new generation's
    /// in-memory input queue starts empty. Document/journal/checkpoint state
    /// is untouched -- this is the primitive
    /// [`Host::replace_generation_with_handoff`] and [`Host::handle_overflow`]
    /// both build on.
    pub fn replace_generation(&mut self) {
        self.generation = GenerationId(self.generation.0 + 1);
        self.input_queue.clear();
        self.guest_consumed_sequence = None;
    }

    /// Replaces the generation, returning the recovery state as of the
    /// instant before cleanup runs.
    ///
    /// This return value, not a later independent call to [`recover`], is
    /// the handoff: cleanup prunes the journal without first checkpointing
    /// it, so a *fresh* `recover()` call issued after this returns is not
    /// guaranteed to see the same thing -- the caller (standing in for
    /// whatever hands a new generation its starting document) must use the
    /// value returned here directly, not re-derive it from disk a moment
    /// later.
    ///
    /// The order between the two steps is still load-bearing, and is the
    /// entire content of the "recovery cleanup runs before replacement
    /// guest has consumed it" mutant: recovery state is *read* before
    /// cleanup prunes the journal, never after. Pruning first and reading
    /// second would return a document missing whatever this generation had
    /// written since the last checkpoint -- present on disk right up until
    /// cleanup deleted it, and gone by the time anything looked, which is
    /// strictly worse than the current design's "gone a moment later than
    /// that instead."
    pub fn replace_generation_with_handoff(&mut self) -> io::Result<RecoveredState> {
        let handoff = recovery::recover(&self.journal_path(), &self.checkpoint_path())?;
        self.cleanup_after_generation()?;
        self.replace_generation();
        Ok(handoff)
    }

    /// Prunes the journal unconditionally. Edits since the last checkpoint
    /// are retired here on the assumption that whatever needed to read them
    /// already has -- true only because
    /// [`Host::replace_generation_with_handoff`] reads before calling this,
    /// and false the moment some future caller does not.
    fn cleanup_after_generation(&mut self) -> io::Result<()> {
        self.journal.truncate()?;
        self.edits_since_checkpoint = 0;
        Ok(())
    }
}
