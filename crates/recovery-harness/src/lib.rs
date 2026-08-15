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
//! # Two layers, on purpose
//!
//! [`RecoveryStore`] is a generic, opaque mechanism: scope directory, record
//! bounds, ordered sequence framing, append, atomic checkpoint replacement,
//! durability, corrupt-tail detection, tail repair, checkpoint-triggered
//! compaction, non-destructive read, discard. It is blind to what it
//! stores -- bytes and a sequence number -- and never decodes, diffs,
//! merges, or applies them, mirroring the opaque `checkpoint.write(slot,
//! seq, bytes, durability)` capability in `docs/adr/0002-generic-recovery-checkpoint.md`.
//!
//! [`fake_guest::FakeGuest`] is the one place in this crate allowed to
//! interpret those bytes as a document: it owns edit semantics, the
//! in-memory document, application sequence, generation lifecycle, and the
//! input-overflow policy, and it is the only thing that ever concatenates a
//! payload onto a string. An earlier version of this harness put a
//! `document: String` and `apply_edit` directly on what this file now calls
//! `RecoveryStore`, which reproduced -- even in test code -- the exact
//! host-owned-document architecture `docs/adr/0001-userland-text-authority.md`
//! rejects. This split exists to stop that from happening again.
//!
//! # What is, and is not, configurable
//!
//! [`RecoveryPolicy`] is the *only* thing a test or a future decision-maker
//! chooses about `FakeGuest`'s behavior -- whether to journal every edit,
//! whether to fsync, how often to checkpoint, what an input-queue overflow
//! does to an existing checkpoint. [`StoreBounds`] is the only thing
//! configured on `RecoveryStore`, and it bounds resource usage, not
//! semantics: whether a torn record is detected, whether a read mutates
//! anything, and how compaction is sequenced are fixed correctness
//! properties of the mechanism, never policy knobs.

pub mod checkpoint;
pub mod checksum;
pub mod fake_guest;
pub mod journal;
pub mod recovery;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

pub use fake_guest::{FakeGuest, RecoveredDocument};
pub use journal::TailFault;
pub use recovery::{RecoveredRecords, SequenceGap};

/// Resource limits `RecoveryStore` enforces before constructing or writing
/// anything, replacing what used to be an implicit, unchecked `usize as
/// u32` cast standing in for a bound that was never actually named.
/// Directly required by ADR 0002's "namespaced, bounded (per-write size
/// cap, per-scope slot-count cap)" language -- the per-write caps are here;
/// this harness models one document per scope directory, so a per-scope
/// slot-count cap has no test surface yet and is left for whichever design
/// adds multi-slot scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreBounds {
    /// Largest single journal record payload, in bytes.
    pub max_record_bytes: usize,
    /// Largest single checkpoint payload, in bytes.
    pub max_checkpoint_bytes: usize,
    /// Largest total on-disk journal size, in bytes, before an append is
    /// refused. Exists so an unbounded run without checkpoints cannot grow
    /// the journal without limit; a caller that hits this is expected to
    /// checkpoint (which prunes the journal) rather than keep appending.
    pub max_journal_bytes: u64,
}

impl StoreBounds {
    pub const DEFAULT: Self = Self {
        max_record_bytes: 4 * 1024 * 1024,
        max_checkpoint_bytes: 32 * 1024 * 1024,
        max_journal_bytes: 64 * 1024 * 1024,
    };
}

impl Default for StoreBounds {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Why a [`RecoveryStore`] operation was refused.
#[derive(Debug)]
pub enum StoreError {
    /// The store found a torn journal tail (on open, or as the result of a
    /// failed append) and refuses further appends until [`RecoveryStore::repair`]
    /// runs. Checkpoint writes are still permitted while poisoned: a
    /// checkpoint never touches the journal until *after* it is durably
    /// installed, so it cannot make a torn journal any worse, and a
    /// successful checkpoint plus a subsequent `repair` is a legitimate way
    /// out of poisoning.
    Poisoned,
    RecordTooLarge {
        len: usize,
        max: usize,
    },
    CheckpointTooLarge {
        len: usize,
        max: usize,
    },
    JournalBudgetExceeded {
        would_be_bytes: u64,
        max_bytes: u64,
    },
    /// `append`'s `sequence` was not exactly one more than the last
    /// sequence this store has durably recorded (via journal or
    /// checkpoint). A pure ordering check on the sequence number itself --
    /// it requires no interpretation of the payload, so the store can
    /// enforce it without knowing what a record means.
    SequenceMismatch {
        expected: u64,
        found: u64,
    },
    Io(io::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Poisoned => write!(f, "recovery store is poisoned; call repair() first"),
            Self::RecordTooLarge { len, max } => {
                write!(f, "record of {len} bytes exceeds max_record_bytes ({max})")
            }
            Self::CheckpointTooLarge { len, max } => write!(
                f,
                "checkpoint of {len} bytes exceeds max_checkpoint_bytes ({max})"
            ),
            Self::JournalBudgetExceeded {
                would_be_bytes,
                max_bytes,
            } => write!(
                f,
                "journal would grow to {would_be_bytes} bytes, exceeding max_journal_bytes ({max_bytes})"
            ),
            Self::SequenceMismatch { expected, found } => write!(
                f,
                "append sequence {found} is not contiguous with the store's last \
                 sequence (expected {expected})"
            ),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A generic, opaque recovery mechanism for one document-shaped scope: an
/// append-only journal of records plus a periodically-replaced full
/// checkpoint. Every method takes or returns raw bytes and a sequence
/// number; nothing here ever looks inside a payload.
pub struct RecoveryStore {
    dir: PathBuf,
    journal: journal::JournalWriter,
    bounds: StoreBounds,
    poisoned: bool,
    /// The last sequence this store has durably recorded, via journal or
    /// checkpoint, whichever is newer. `append` requires the next call to
    /// name exactly `last_sequence + 1`; this is what makes a gap or a
    /// duplicate a refusal at write time, not just something a later read
    /// notices.
    last_sequence: u64,
}

impl RecoveryStore {
    /// Opens (creating if necessary) the recovery state under `dir`. If a
    /// torn journal tail already exists on disk -- left by a previous
    /// process that died mid-append -- the store opens poisoned, refusing
    /// further appends until [`Self::repair`] runs; this is the same state
    /// a fresh in-process append failure produces, so both cases are
    /// handled by the same recovery path.
    pub fn open(dir: impl Into<PathBuf>, bounds: StoreBounds) -> io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        checkpoint::discard_stale_tmp(&dir)?;

        let journal_path = Self::journal_path_for(&dir);
        let checkpoint_path = Self::checkpoint_path_for(&dir);
        let existing_checkpoint = checkpoint::read_checkpoint_file(&checkpoint_path)?;
        let existing_journal = journal::read_journal_file(&journal_path)?;
        let poisoned = existing_journal.tail_fault.is_some();
        let last_sequence = existing_journal
            .records
            .last()
            .map(|r| r.sequence)
            .or_else(|| existing_checkpoint.map(|c| c.sequence))
            .unwrap_or(0);

        let journal = journal::JournalWriter::open(&journal_path)?;
        Ok(Self {
            dir,
            journal,
            bounds,
            poisoned,
            last_sequence,
        })
    }

    pub fn journal_path_for(dir: &Path) -> PathBuf {
        dir.join("journal.bin")
    }

    pub fn checkpoint_path_for(dir: &Path) -> PathBuf {
        checkpoint::checkpoint_path_for(dir)
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

    pub fn bounds(&self) -> StoreBounds {
        self.bounds
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn journal_size_bytes(&self) -> io::Result<u64> {
        self.journal.len_bytes()
    }

    /// Appends one opaque record. Refuses with `StoreError::Poisoned`
    /// without touching the file at all if the store is already poisoned,
    /// and with a bound error, also without touching the file, if `payload`
    /// or the resulting journal size would exceed `self.bounds`.
    ///
    /// Any *write* failure -- injected via [`Self::journal_writer_mut`], or
    /// real -- poisons the store. The file may now hold a torn record even
    /// though this call also returns `Err` without having updated any of
    /// this struct's own fields; "the store is unchanged" is not a claim
    /// this function is willing to make about a write that failed, because
    /// the filesystem changed even when nothing in memory did.
    pub fn append(&mut self, sequence: u64, payload: &[u8], fsync: bool) -> Result<(), StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        let expected = self.last_sequence + 1;
        if sequence != expected {
            return Err(StoreError::SequenceMismatch {
                expected,
                found: sequence,
            });
        }
        if payload.len() > self.bounds.max_record_bytes {
            return Err(StoreError::RecordTooLarge {
                len: payload.len(),
                max: self.bounds.max_record_bytes,
            });
        }
        let current_len = self.journal.len_bytes()?;
        let would_be_bytes = current_len + journal::JournalRecord::wire_len(payload.len()) as u64;
        if would_be_bytes > self.bounds.max_journal_bytes {
            return Err(StoreError::JournalBudgetExceeded {
                would_be_bytes,
                max_bytes: self.bounds.max_journal_bytes,
            });
        }

        match self.journal.append(sequence, payload, fsync) {
            Ok(()) => {
                self.last_sequence = sequence;
                Ok(())
            }
            Err(error) => {
                self.poisoned = true;
                Err(StoreError::Io(error))
            }
        }
    }

    /// Writes a full checkpoint at `sequence`, atomically, and only on
    /// success prunes the journal -- the *only* operation permitted to
    /// prune it. See [`checkpoint::write_checkpoint_atomic`] for the
    /// durability guarantee: a failure here (including an injected one via
    /// [`Self::write_checkpoint_faulted`]) never touches the previous
    /// checkpoint or the journal, so it does not poison the store.
    pub fn write_checkpoint(&mut self, sequence: u64, content: &[u8]) -> Result<(), StoreError> {
        self.write_checkpoint_inner(sequence, content, checkpoint::WriteFault::None)
    }

    /// Test-only: injects a fault into the checkpoint's temp-file write, to
    /// prove the atomic-replacement guarantee without needing a subprocess
    /// for every such case.
    pub fn write_checkpoint_faulted(
        &mut self,
        sequence: u64,
        content: &[u8],
        fault: checkpoint::WriteFault,
    ) -> Result<(), StoreError> {
        self.write_checkpoint_inner(sequence, content, fault)
    }

    fn write_checkpoint_inner(
        &mut self,
        sequence: u64,
        content: &[u8],
        fault: checkpoint::WriteFault,
    ) -> Result<(), StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        if content.len() > self.bounds.max_checkpoint_bytes {
            return Err(StoreError::CheckpointTooLarge {
                len: content.len(),
                max: self.bounds.max_checkpoint_bytes,
            });
        }
        let checkpoint = checkpoint::Checkpoint {
            sequence,
            content: content.to_vec(),
        };
        checkpoint::write_checkpoint_atomic(&self.dir, &checkpoint, fault)?;

        // Reached only if the checkpoint is now durably installed: pruning
        // the journal here can never discard the only copy of anything,
        // because the checkpoint that subsumes it is already safe on disk.
        if let Err(error) = self.journal.truncate() {
            self.poisoned = true;
            return Err(StoreError::Io(error));
        }
        self.last_sequence = self.last_sequence.max(sequence);
        Ok(())
    }

    /// Non-destructive: reads whatever is currently on disk without
    /// mutating either file. Calling this at any point, including while
    /// poisoned, never changes what a subsequent call sees.
    pub fn read(&self) -> io::Result<RecoveredRecords> {
        recovery::read(&self.journal_path(), &self.checkpoint_path())
    }

    /// Truncates the journal to its longest trustworthy prefix and clears
    /// the poisoned state. A no-op on the poisoned flag (but still clears
    /// it) if the on-disk tail turns out to be healthy after all -- e.g. a
    /// checkpoint write poisoned nothing but a caller called `repair`
    /// speculatively anyway.
    pub fn repair(&mut self) -> Result<(), StoreError> {
        let read = journal::read_journal_file(&self.journal_path())?;
        if read.tail_fault.is_some() {
            self.journal.truncate_to(read.trusted_len())?;
        }
        // Re-derive the resume point from whatever survived the repair,
        // never from what was there before the tear -- the torn record
        // itself never counted as durable.
        let checkpoint = checkpoint::read_checkpoint_file(&self.checkpoint_path())?;
        self.last_sequence = read
            .records
            .last()
            .map(|r| r.sequence)
            .or_else(|| checkpoint.map(|c| c.sequence))
            .unwrap_or(0);
        self.poisoned = false;
        Ok(())
    }

    /// Deletes all recovery state for this scope: journal and checkpoint.
    /// No policy in this harness calls this automatically -- included
    /// because ADR 0002 names `checkpoint.discard(slot)` as part of the
    /// mechanism's own surface, even though *when* to discard is a
    /// lifecycle decision this store deliberately has no opinion on.
    pub fn discard(&mut self) -> io::Result<()> {
        self.journal.truncate()?;
        checkpoint::delete(&self.checkpoint_path())?;
        self.poisoned = false;
        self.last_sequence = 0;
        Ok(())
    }

    /// Test-only access to the journal writer, for fault-injecting the next
    /// append in-process -- proving what a real crash mid-write would leave
    /// behind without needing a subprocess for every such case. Using this
    /// directly bypasses `RecoveryStore::append`'s bookkeeping (bounds
    /// checks, poisoning-on-failure), so tests that want those to run
    /// should prefer `append` with `journal::WriteFault` armed via this
    /// accessor and then still call `append`.
    pub fn journal_writer_mut(&mut self) -> &mut journal::JournalWriter {
        &mut self.journal
    }
}

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
