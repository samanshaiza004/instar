//! What one generation still has to be told about one buffer (C1).
//!
//! The host edits its replica immediately and the guest hears afterwards. That
//! is the whole of Phase 3's opening decision, and this is the state that makes
//! "afterwards" bounded rather than open-ended:
//!
//! ```text
//! host-local edit  ->  SyncState::record   ->  the guest drains it later (C2)
//! ```
//!
//! # Why a stalled guest is survivable
//!
//! A queue that grows with the stall is a queue an unresponsive guest can use
//! to exhaust the host — and the second Phase 3 gate says plainly that a
//! stalled guest must not stall the caret, which means the host cannot answer
//! back-pressure by refusing to edit. So the queue has a ceiling, and past it
//! the history is **discarded** rather than trimmed:
//!
//! ```text
//! Queued          the guest can still be brought up to date incrementally
//! Desynchronized  it cannot, and must re-read; cost is one revision number
//! ```
//!
//! Past the ceiling the host's cost stops growing entirely. That is the
//! property, and it holds no matter how long the guest is gone.
//!
//! # Collapse, never coalesce
//!
//! A tempting third option is to merge the backlog into one edit spanning
//! `min..max` of everything it touched. It is rejected: that span is usually
//! most of the document anyway, and it destroys the exact edit granularity
//! Tree-sitter and every other incremental consumer exists to use. One
//! incremental path plus one snapshot recovery path is fewer algorithms than an
//! incremental path, a synthetic-edit path, and a recovery path — and the
//! recovery path has to exist regardless.
//!
//! # `latest_revision` is not inside the variants
//!
//! `docs/PHASE-3.md` draws the state as two variants with the revision living
//! in `Desynchronized`. It is hoisted out here, because it is meaningful in
//! both states — the desync marker carries it, and resynchronization re-arms
//! *at* it — and hoisting means the collapse cannot lose it by construction.
//! One less thing for a later edit to get wrong.

use std::collections::VecDeque;

use instar_text::{AppliedEdit, Revision};

/// How many pending edits one generation may owe on one buffer.
///
/// Sized for a stall, not for a document: at ordinary typing speed this is
/// minutes of accumulated keystrokes, so an ordinarily slow guest stays on the
/// incremental path and only a genuinely absent one falls off it.
pub const MAX_PENDING_EDITS: usize = 4_096;

/// How many bytes of replacement text those edits may carry.
///
/// `textbench` exercises a 100 KB paste, so the ceiling has to sit well above
/// one of those and still well below a runaway. A count-only bound would be
/// defeated by a single paste and a byte-only bound by ten thousand
/// keystrokes; **neither number is the bound, the pair is.**
pub const MAX_PENDING_EDIT_BYTES: usize = 1 << 20;

/// Whether the guest can still be caught up incrementally.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    Queued {
        queue: VecDeque<AppliedEdit>,
        bytes: usize,
    },
    /// History was discarded. Nothing accumulates here, which is the point.
    Desynchronized,
}

/// One generation's synchronization state for one buffer.
///
/// Keyed by `(GenerationId, TextBufferId)` where it is stored, so a
/// generation's death takes its synchronization state and nothing else — the
/// same rule its leases already follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncState {
    /// The newest revision of the buffer, tracked in both states.
    latest_revision: Revision,
    pending: Pending,
    /// The most bytes ever queued at once.
    ///
    /// An instrument, and the only way to state the check-before-allocation
    /// rule as something a test can fail. "Push, then check the length" and
    /// "check, then push" reach an identical final state — desynchronized,
    /// queue cleared — so no assertion about the *outcome* can tell them
    /// apart. The high-water mark can: the first briefly holds a queue larger
    /// than the ceiling, and the second never does.
    peak_bytes: usize,
}

impl SyncState {
    /// A buffer the guest is up to date with, as every buffer begins.
    ///
    /// `create-buffer(contents)` and `create-empty-buffer` both land here: the
    /// guest supplied the contents or knows they are empty, so it is
    /// synchronized at the baseline with nothing owed. Bootstrap establishes a
    /// revision, never an edit.
    pub fn synchronized(baseline: Revision) -> Self {
        Self {
            latest_revision: baseline,
            pending: Pending::Queued {
                queue: VecDeque::new(),
                bytes: 0,
            },
            peak_bytes: 0,
        }
    }

    pub fn latest_revision(&self) -> Revision {
        self.latest_revision
    }

    pub fn is_synchronized(&self) -> bool {
        matches!(self.pending, Pending::Queued { .. })
    }

    pub fn queued(&self) -> usize {
        match &self.pending {
            Pending::Queued { queue, .. } => queue.len(),
            Pending::Desynchronized => 0,
        }
    }

    /// The most bytes ever queued at once. See the field.
    pub fn peak_queued_bytes(&self) -> usize {
        self.peak_bytes
    }

    pub fn queued_bytes(&self) -> usize {
        match &self.pending {
            Pending::Queued { bytes, .. } => *bytes,
            Pending::Desynchronized => 0,
        }
    }

    /// Records one host-local edit.
    ///
    /// The revision advances in both states — while desynchronized it is the
    /// *only* thing that moves, and it is what resynchronization will re-arm
    /// at, so losing it would leave the guest re-armed at a revision the
    /// buffer had already passed.
    ///
    /// # The bound is checked before the allocation
    ///
    /// Load-bearing, and the reason this is not `push` followed by a length
    /// test: an edit carrying a megabyte would be cloned into the queue and
    /// *then* found to have exceeded the ceiling, which allocates precisely
    /// the memory the ceiling exists to prevent. The prospective totals are
    /// computed first, and an edit that would not fit is never cloned at all.
    pub fn record(&mut self, applied: &AppliedEdit) {
        self.latest_revision = applied.resulting_revision;

        let Pending::Queued { queue, bytes } = &mut self.pending else {
            // Desynchronized: the revision moved above, and nothing else does.
            return;
        };

        let prospective_count = queue.len() + 1;
        let prospective_bytes = bytes.checked_add(applied.edit.replacement.len());

        let fits = prospective_count <= MAX_PENDING_EDITS
            && prospective_bytes.is_some_and(|total| total <= MAX_PENDING_EDIT_BYTES);

        if !fits {
            // Discard, do not trim. A queue missing its head is a history that
            // claims to be whole and is not, and applying it would corrupt the
            // guest's document more quietly than losing it does.
            self.pending = Pending::Desynchronized;
            return;
        }

        *bytes = prospective_bytes.expect("checked above");
        queue.push_back(applied.clone());
        self.peak_bytes = self.peak_bytes.max(*bytes);
    }

    /// Takes up to `max_entries` pending edits, oldest first.
    ///
    /// `None` when there is nothing to report and the guest should suspend.
    /// A desynchronized state never reports edits — the caller asks
    /// [`SyncState::is_synchronized`] and delivers the marker instead — and
    /// **draining does not re-arm anything**: recovery is a property of the
    /// read, not of the delivery, so a `SyncState` stays desynchronized until
    /// [`SyncState::resynchronize`] is called with a revision the guest has
    /// actually seen.
    pub fn take_batch(&mut self, max_entries: usize) -> Option<Vec<AppliedEdit>> {
        let Pending::Queued { queue, bytes } = &mut self.pending else {
            return None;
        };
        if queue.is_empty() {
            return None;
        }
        let take = max_entries.min(queue.len());
        let batch: Vec<AppliedEdit> = queue.drain(..take).collect();
        *bytes -= batch
            .iter()
            .map(|applied| applied.edit.replacement.len())
            .sum::<usize>();
        Some(batch)
    }

    /// Re-arms at a revision the guest has authoritatively read.
    ///
    /// The only exit from [`Pending::Desynchronized`], and legal while
    /// synchronized too: a guest that would rather not replay a backlog it has
    /// just read past may discard it the same way. One snapshot mechanism
    /// serves both, which is also what keeps the recovery path exercised by
    /// ordinary use rather than only by faults.
    ///
    /// Callers must perform the read and this call as one operation on the
    /// thread that owns the text subsystem. An edit landing between them would
    /// be dropped without trace, and the guest would believe itself
    /// synchronized at a revision it had never been told about.
    pub fn resynchronize(&mut self, revision: Revision) {
        self.latest_revision = revision;
        self.pending = Pending::Queued {
            queue: VecDeque::new(),
            bytes: 0,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use instar_text::TextEdit;

    /// An applied edit that inserts `len` bytes, moving `rev` to `rev + 1`.
    fn edit_of(rev: u64, len: usize) -> AppliedEdit {
        AppliedEdit {
            base_revision: Revision(rev),
            resulting_revision: Revision(rev + 1),
            edit: TextEdit::insert(0, "x".repeat(len)),
        }
    }

    fn record_n(state: &mut SyncState, count: usize, len: usize) {
        for i in 0..count {
            state.record(&edit_of(i as u64, len));
        }
    }

    #[test]
    fn a_fresh_state_is_synchronized_and_owes_nothing() {
        let state = SyncState::synchronized(Revision(7));
        assert!(state.is_synchronized());
        assert_eq!(state.latest_revision(), Revision(7));
        assert_eq!(state.queued(), 0);
        assert_eq!(state.queued_bytes(), 0);
    }

    #[test]
    fn ordinary_edits_queue_in_order() {
        let mut state = SyncState::synchronized(Revision(0));
        record_n(&mut state, 3, 1);

        assert!(state.is_synchronized());
        assert_eq!(state.queued(), 3);
        assert_eq!(state.latest_revision(), Revision(3));

        let batch = state.take_batch(16).expect("three are pending");
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].base_revision, Revision(0));
        assert_eq!(batch[2].resulting_revision, Revision(3));
        assert_eq!(state.queued_bytes(), 0, "draining releases the bytes too");
    }

    /// The count ceiling. One edit past it discards everything.
    #[test]
    fn one_edit_past_the_count_ceiling_desynchronizes() {
        let mut state = SyncState::synchronized(Revision(0));
        record_n(&mut state, MAX_PENDING_EDITS, 1);
        assert!(state.is_synchronized(), "exactly at the ceiling still fits");
        assert_eq!(state.queued(), MAX_PENDING_EDITS);

        state.record(&edit_of(MAX_PENDING_EDITS as u64, 1));

        assert!(!state.is_synchronized());
        assert_eq!(state.queued(), 0, "discarded, not trimmed");
    }

    /// The byte ceiling, which the count ceiling cannot stand in for: a single
    /// paste defeats a count-only bound outright.
    #[test]
    fn one_paste_past_the_byte_ceiling_desynchronizes() {
        let mut state = SyncState::synchronized(Revision(0));
        state.record(&edit_of(0, MAX_PENDING_EDIT_BYTES));
        assert!(state.is_synchronized(), "exactly at the ceiling still fits");
        assert_eq!(state.queued(), 1);

        state.record(&edit_of(1, 1));

        assert!(
            !state.is_synchronized(),
            "one byte past the cap, on the second entry of four thousand \
             allowed -- a count-only bound would not have noticed"
        );
    }

    /// And the other direction: many small edits defeat a byte-only bound.
    #[test]
    fn many_small_edits_reach_the_count_ceiling_well_under_the_byte_one() {
        let mut state = SyncState::synchronized(Revision(0));
        record_n(&mut state, MAX_PENDING_EDITS + 1, 1);

        assert!(!state.is_synchronized());
        assert!(
            MAX_PENDING_EDITS < MAX_PENDING_EDIT_BYTES,
            "this test only says something while the count ceiling is the one \
             a keystroke stream reaches first"
        );
    }

    /// The property the whole state exists for: an arbitrarily long stall
    /// costs the host a revision number and nothing else.
    #[test]
    fn a_desynchronized_state_accumulates_nothing_however_long_the_stall() {
        let mut state = SyncState::synchronized(Revision(0));
        record_n(&mut state, MAX_PENDING_EDITS + 1, 1);
        assert!(!state.is_synchronized());

        for i in 0..100_000u64 {
            state.record(&edit_of(1_000 + i, 64));
        }

        assert_eq!(state.queued(), 0);
        assert_eq!(state.queued_bytes(), 0);
        assert_eq!(
            state.latest_revision(),
            Revision(1_000 + 100_000),
            "the revision still tracks, because resynchronization re-arms at it"
        );
    }

    /// Draining the queue is not recovery, and neither is draining the marker.
    #[test]
    fn desynchronization_survives_being_observed() {
        let mut state = SyncState::synchronized(Revision(0));
        record_n(&mut state, MAX_PENDING_EDITS + 1, 1);

        assert!(state.take_batch(16).is_none());
        assert!(
            !state.is_synchronized(),
            "asking for a batch must not re-arm: an edit landing between the \
             guest hearing it is behind and the guest re-reading would vanish"
        );
    }

    #[test]
    fn resynchronizing_re_arms_at_the_revision_the_guest_read() {
        let mut state = SyncState::synchronized(Revision(0));
        record_n(&mut state, MAX_PENDING_EDITS + 1, 1);
        assert!(!state.is_synchronized());

        state.resynchronize(Revision(9_000));

        assert!(state.is_synchronized());
        assert_eq!(state.latest_revision(), Revision(9_000));
        assert_eq!(state.queued(), 0);

        state.record(&edit_of(9_000, 4));
        assert_eq!(state.queued(), 1, "and edits queue normally again");
    }

    /// Legal while synchronized, which is what lets a guest discard a backlog
    /// it has just read past instead of replaying it.
    #[test]
    fn resynchronizing_while_synchronized_discards_the_backlog() {
        let mut state = SyncState::synchronized(Revision(0));
        record_n(&mut state, 5, 8);
        assert_eq!(state.queued(), 5);

        state.resynchronize(Revision(5));

        assert!(state.is_synchronized());
        assert_eq!(state.queued(), 0);
        assert_eq!(state.queued_bytes(), 0);
    }

    #[test]
    fn a_batch_is_bounded_and_the_rest_stays_queued() {
        let mut state = SyncState::synchronized(Revision(0));
        record_n(&mut state, 10, 2);

        let batch = state.take_batch(4).expect("ten are pending");
        assert_eq!(batch.len(), 4);
        assert_eq!(state.queued(), 6);
        assert_eq!(
            state.queued_bytes(),
            12,
            "the released bytes are the drained ones, not all of them"
        );
    }

    /// The queue never briefly holds more than the ceiling allows.
    ///
    /// This is the check-before-allocation rule, and it needs the high-water
    /// mark to be sayable at all: "push, then check" and "check, then push"
    /// reach the same final state, so only peak occupancy separates them. An
    /// implementation that clones a megabyte into the queue before noticing it
    /// does not fit has already spent the memory the ceiling exists to refuse.
    #[test]
    fn the_queue_never_exceeds_the_ceiling_even_briefly() {
        let mut state = SyncState::synchronized(Revision(0));
        state.record(&edit_of(0, MAX_PENDING_EDIT_BYTES));
        assert_eq!(state.peak_queued_bytes(), MAX_PENDING_EDIT_BYTES);

        // Would take it to twice the ceiling if it were admitted first.
        state.record(&edit_of(1, MAX_PENDING_EDIT_BYTES));

        assert!(!state.is_synchronized());
        assert_eq!(
            state.peak_queued_bytes(),
            MAX_PENDING_EDIT_BYTES,
            "the refused edit was never cloned into the queue"
        );
    }

    #[test]
    fn nothing_pending_reports_nothing() {
        let mut state = SyncState::synchronized(Revision(3));
        assert!(state.take_batch(16).is_none());
        assert!(state.is_synchronized());
    }
}
