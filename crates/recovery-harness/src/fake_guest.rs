//! `FakeGuest`: the only place in this crate that knows a recovery payload
//! is UTF-8 text meant to be appended to a document.
//!
//! Everything below [`RecoveryStore`] deals in opaque bytes and sequence
//! numbers. `FakeGuest` is what turns "append these bytes" into "the
//! document now reads this", and it owns every fact that requires that
//! interpretation: the live in-memory document, the application sequence,
//! generation identity, the input queue, and what an input-queue overflow
//! does. It stands in for the guest-owned application model
//! `docs/adr/0001-userland-text-authority.md` describes -- a real guest
//! would own its own edit semantics and undo stack the same way this one
//! owns string concatenation, just with more of both.

use std::collections::VecDeque;
use std::io;
use std::path::Path;

use crate::{
    CheckpointTrigger, EditOutcome, GenerationId, InputEvent, InputOverflow, JournalMode,
    OverflowAction, RecoveryPolicy, RecoveryStore, SequenceGap, StoreBounds, StoreError,
    TailFault, checkpoint, recovery,
};

/// The result of reconstructing a document from whatever a [`RecoveryStore`]
/// currently holds. The one place raw journal/checkpoint bytes become a
/// `String` -- built by [`FakeGuest::recover_document`], never by
/// `recovery::read` itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredDocument {
    pub content: String,
    /// The newest edit sequence recovery is willing to vouch for -- what a
    /// future doc should call the recovery sequence, distinct from whatever
    /// sequence was last shown on screen before the failure.
    pub last_recovered_sequence: u64,
    /// How many journal records were actually folded into `content`, after
    /// skipping anything already covered by the checkpoint. A mutant that
    /// replays or drops a record changes this count without necessarily
    /// changing `content` in a way that's obvious by inspection, which is
    /// why it is reported as its own fact rather than left implicit.
    pub applied_from_journal: usize,
    pub tail_fault: Option<TailFault>,
    pub tail_offset: Option<u64>,
    /// `Some` if the journal's sequence numbers were not contiguous from
    /// the checkpoint forward -- a record is apparently missing. `content`
    /// only reflects records up to the gap.
    pub sequence_gap: Option<SequenceGap>,
}

/// The host-adjacent half of one document: exactly the state a real Instar
/// host process would hold in memory for a guest-owned document, plus the
/// [`RecoveryStore`] a chosen [`RecoveryPolicy`] might add underneath it.
pub struct FakeGuest {
    store: RecoveryStore,
    policy: RecoveryPolicy,
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

impl FakeGuest {
    pub fn open(dir: impl Into<std::path::PathBuf>, policy: RecoveryPolicy) -> io::Result<Self> {
        let store = RecoveryStore::open(dir, StoreBounds::DEFAULT)?;
        Ok(Self {
            store,
            policy,
            document: String::new(),
            sequence: 0,
            edits_since_checkpoint: 0,
            generation: GenerationId(1),
            input_queue: VecDeque::new(),
            guest_consumed_sequence: None,
        })
    }

    pub fn open_with_bounds(
        dir: impl Into<std::path::PathBuf>,
        policy: RecoveryPolicy,
        bounds: StoreBounds,
    ) -> io::Result<Self> {
        let store = RecoveryStore::open(dir, bounds)?;
        Ok(Self {
            store,
            policy,
            document: String::new(),
            sequence: 0,
            edits_since_checkpoint: 0,
            generation: GenerationId(1),
            input_queue: VecDeque::new(),
            guest_consumed_sequence: None,
        })
    }

    /// Opens against `dir`, bootstrapping the in-memory document and
    /// sequence from whatever is already recovered there, instead of
    /// starting empty. This is what a real host process restart looks
    /// like: `open` is for a document that has never existed before,
    /// `resume` is for one that might already have history on disk.
    ///
    /// Against an empty or nonexistent `dir` this produces exactly what
    /// `open` would, so every existing single-cycle test that used `open`
    /// against a fresh directory is unaffected by using `resume` instead --
    /// the two only diverge once there is prior history to pick up.
    ///
    /// If the recovered state has a tail fault or a sequence gap, the
    /// document is bootstrapped from whatever survived up to that point,
    /// and the underlying `RecoveryStore` opens poisoned exactly as `open`
    /// would in the tail-fault case -- `apply_edit` refuses until
    /// `repair()` runs.
    pub fn resume(dir: impl Into<std::path::PathBuf>, policy: RecoveryPolicy) -> io::Result<Self> {
        Self::resume_with_bounds(dir, policy, StoreBounds::DEFAULT)
    }

    pub fn resume_with_bounds(
        dir: impl Into<std::path::PathBuf>,
        policy: RecoveryPolicy,
        bounds: StoreBounds,
    ) -> io::Result<Self> {
        let dir = dir.into();
        let recovered = Self::recover_document(&dir)?;
        let store = RecoveryStore::open(&dir, bounds)?;
        Ok(Self {
            store,
            policy,
            document: recovered.content,
            sequence: recovered.last_recovered_sequence,
            edits_since_checkpoint: recovered.applied_from_journal as u32,
            generation: GenerationId(1),
            input_queue: VecDeque::new(),
            guest_consumed_sequence: None,
        })
    }

    pub fn journal_path_for(dir: &Path) -> std::path::PathBuf {
        RecoveryStore::journal_path_for(dir)
    }

    pub fn checkpoint_path_for(dir: &Path) -> std::path::PathBuf {
        RecoveryStore::checkpoint_path_for(dir)
    }

    pub fn journal_path(&self) -> std::path::PathBuf {
        self.store.journal_path()
    }

    pub fn checkpoint_path(&self) -> std::path::PathBuf {
        self.store.checkpoint_path()
    }

    pub fn dir(&self) -> &Path {
        self.store.dir()
    }

    pub fn policy(&self) -> RecoveryPolicy {
        self.policy
    }

    pub fn generation(&self) -> GenerationId {
        self.generation
    }

    pub fn is_poisoned(&self) -> bool {
        self.store.is_poisoned()
    }

    /// The document as this live process currently has it -- what would be
    /// on screen. Distinct from what `recover_document` would reconstruct
    /// from disk, which may lag it under any policy weaker than
    /// `JOURNAL_EVERY_EDIT_DURABLE`.
    pub fn document(&self) -> &str {
        &self.document
    }

    /// "Last presented sequence": the newest edit this live process has
    /// applied to its own in-memory document, independent of what is
    /// durable. Captured by a test *before* injecting a failure, then
    /// compared against `RecoveredDocument::last_recovered_sequence` after,
    /// to state precisely how much a given policy allowed to be lost.
    pub fn last_presented_sequence(&self) -> u64 {
        self.sequence
    }

    pub fn mark_guest_consumed(&mut self, sequence: u64) {
        self.guest_consumed_sequence = Some(sequence);
    }

    pub fn guest_consumed_sequence(&self) -> Option<u64> {
        self.guest_consumed_sequence
    }

    /// Test-only access to the underlying store's journal writer, for
    /// fault-injecting the next append in-process.
    pub fn journal_writer_mut(&mut self) -> &mut crate::journal::JournalWriter {
        self.store.journal_writer_mut()
    }

    pub fn journal_size_bytes(&self) -> io::Result<u64> {
        self.store.journal_size_bytes()
    }

    /// The one edit path. A journal write that fails leaves `self`'s
    /// in-memory document and sequence unchanged -- nothing below the
    /// write may run until it has returned `Ok`, which is what "mark edit
    /// durable before write completes" would violate. The underlying
    /// `RecoveryStore`, however, may now be poisoned: a failed write can
    /// leave a torn record in the journal file even though this struct's
    /// own fields never moved, so a caller checking "did anything change"
    /// must check `is_poisoned()`, not only `document()`.
    pub fn apply_edit(&mut self, delta: impl Into<String>) -> Result<EditOutcome, StoreError> {
        let delta = delta.into();
        let candidate_sequence = self.sequence + 1;

        let durable = if let JournalMode::AppendEveryEdit { fsync } = self.policy.journal {
            self.store
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

    /// Writes a full-document checkpoint at the current sequence. Pruning
    /// the journal afterward is `RecoveryStore::write_checkpoint`'s job,
    /// not this method's, and it happens only after the checkpoint is
    /// durably installed.
    pub fn checkpoint(&mut self) -> Result<(), StoreError> {
        self.store
            .write_checkpoint(self.sequence, self.document.as_bytes())?;
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
    /// in-memory input queue starts empty. Document/journal/checkpoint
    /// state on disk is untouched -- nothing about a generation ending
    /// prunes or otherwise retires recovery data. That is
    /// `RecoveryStore::write_checkpoint`'s job alone, and it happens only
    /// when a checkpoint has actually subsumed what it prunes.
    pub fn replace_generation(&mut self) {
        self.generation = GenerationId(self.generation.0 + 1);
        self.input_queue.clear();
        self.guest_consumed_sequence = None;
    }

    /// Replaces the generation and returns what recovery currently shows.
    ///
    /// Unlike an earlier version of this method, this is now a genuinely
    /// non-destructive read followed by a generation bump: nothing here
    /// prunes the journal, so a *fresh* `recover_document` call issued
    /// after this returns sees exactly the same thing this call returned
    /// (until further edits happen) -- there is no longer a window in
    /// which cleanup could retire data before a replacement guest has
    /// consumed it, because there is no cleanup step here at all.
    pub fn replace_generation_with_handoff(&mut self) -> io::Result<RecoveredDocument> {
        let handoff = Self::recover_document(self.dir())?;
        self.replace_generation();
        Ok(handoff)
    }

    /// Reconstructs a document from whatever `dir` currently holds on disk,
    /// with no side effects. This is the only function in this module (and
    /// in this crate) that turns opaque recovery bytes into a `String` --
    /// the interpretation `recovery::read` deliberately refuses to do.
    pub fn recover_document(dir: &Path) -> io::Result<RecoveredDocument> {
        let records = recovery::read(
            &RecoveryStore::journal_path_for(dir),
            &RecoveryStore::checkpoint_path_for(dir),
        )?;

        let mut content = records
            .checkpoint
            .as_ref()
            .map(|c| String::from_utf8_lossy(&c.content).into_owned())
            .unwrap_or_default();
        let applied_from_journal = records.journal_records.len();
        for record in &records.journal_records {
            content.push_str(&String::from_utf8_lossy(&record.payload));
        }

        Ok(RecoveredDocument {
            content,
            last_recovered_sequence: records.last_recovered_sequence(),
            applied_from_journal,
            tail_fault: records.tail_fault,
            tail_offset: records.tail_offset,
            sequence_gap: records.sequence_gap,
        })
    }

    /// Truncates the underlying journal to its longest trustworthy prefix
    /// and clears poisoning. See `RecoveryStore::repair`.
    pub fn repair(&mut self) -> Result<(), StoreError> {
        self.store.repair()
    }
}
