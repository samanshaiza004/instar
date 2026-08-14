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
use tokio::sync::oneshot;

/// Identifies one registration of one sleeper.
///
/// Monotonic and never reused, so "remove the waiter" can always be stated as
/// "remove *this* waiter" — see [`BufferSync::remove_waiter_if`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct WaiterId(pub u64);

/// One suspended consumer of one buffer's queue.
///
/// Deliberately **not** a semaphore permit, and deliberately not the same type
/// as `CommitPermit`. The two look alike — both say "only one" — and are
/// different protocols:
///
/// ```text
/// CommitPermit      scope GenerationId          admission and exclusion
///                   a second commit is refused because the first is working
/// NextEditWaiter    scope (generation, buffer)  notification and cancellation
///                   a second reader is refused because there is nothing to
///                   read and someone else is already asleep on it
/// ```
///
/// A semaphore is the wrong shape for the second: acquiring one *queues*, and
/// a second `next-edit` must fail immediately rather than line up behind a
/// sleeper it would then race to serve. `Notify` is wrong for a different
/// reason — its cancellation and fairness caveats buy generality that a single
/// explicitly-owned waiter does not need.
#[derive(Debug)]
pub struct NextEditWaiter {
    id: WaiterId,
    wake: oneshot::Sender<()>,
}

impl NextEditWaiter {
    pub fn new(id: WaiterId, wake: oneshot::Sender<()>) -> Self {
        Self { id, wake }
    }

    pub fn id(&self) -> WaiterId {
        self.id
    }

    /// Whether the sleeper has gone away.
    ///
    /// The only way cancellation is observed. A guest dropping its `next-edit`
    /// future drops the receiver, and nothing tells the host — the alternative
    /// would be cross-thread work in a `Drop`, which is exactly the lifecycle
    /// coupling this project spent two phases removing. So the slot is checked
    /// rather than notified, at the two moments it matters: installing a new
    /// waiter, and waking an existing one.
    pub fn is_abandoned(&self) -> bool {
        self.wake.is_closed()
    }

    fn wake(self) {
        // A closed channel means the sleeper left. Nothing to do, and nothing
        // wrong with it.
        let _ = self.wake.send(());
    }
}

/// Why a `next-edit` could not be registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitRefusal {
    /// Another live caller is already asleep on this buffer's queue.
    AlreadyWaiting,
}

/// What a `next-edit` call resolves to without suspending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditNotification {
    Edits(Vec<AppliedEdit>),
    Desynchronized(Revision),
}

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

/// One buffer's synchronization relationship with one generation: what it is
/// owed, and who is asleep waiting to be told.
///
/// The waiter lives beside the state rather than in a registry of its own,
/// because "is there anything to report" and "is anyone listening" are
/// answered together on every path that touches either — an edit arriving, a
/// collapse, a drain, a teardown. Splitting them would mean two lookups that
/// must agree.
#[derive(Debug)]
pub struct BufferSync {
    sync: SyncState,
    waiter: Option<NextEditWaiter>,
}

impl BufferSync {
    pub fn synchronized(baseline: Revision) -> Self {
        Self {
            sync: SyncState::synchronized(baseline),
            waiter: None,
        }
    }

    pub fn state(&self) -> &SyncState {
        &self.sync
    }

    /// Reaches the state machine directly, so a test can drive it to a
    /// specific shape without going through the wake path it is about to
    /// assert on.
    #[cfg(test)]
    fn state_mut_for_test(&mut self) -> &mut SyncState {
        &mut self.sync
    }

    pub fn has_waiter(&self) -> bool {
        self.waiter.is_some()
    }

    /// Records an edit, and wakes a sleeper if this gave it something to say.
    ///
    /// Both transitions wake: a queued edit and a collapse into
    /// [`Pending::Desynchronized`] are equally things the guest is owed. A
    /// collapse that did not wake would leave a sleeper parked until the
    /// *next* edit, and a guest that never hears it has fallen behind is worse
    /// off than one told immediately.
    pub fn record(&mut self, applied: &AppliedEdit) {
        self.sync.record(applied);
        self.wake_if_ready();
    }

    /// Takes what a `next-edit` should return right now, if anything.
    ///
    /// `None` means "nothing to say, suspend" — which is the only case in
    /// which a waiter is installed.
    pub fn poll(&mut self, max_entries: usize) -> Option<EditNotification> {
        if !self.sync.is_synchronized() {
            // Sticky by design: reporting the marker does not clear it. Only
            // an authoritative read re-arms, so a guest that hears it is
            // behind and then reads cannot lose an edit in between.
            return Some(EditNotification::Desynchronized(
                self.sync.latest_revision(),
            ));
        }
        self.sync
            .take_batch(max_entries)
            .map(EditNotification::Edits)
    }

    /// Registers a sleeper, or refuses because a live one already exists.
    ///
    /// An abandoned waiter is evicted rather than counted: a guest that
    /// dropped its future left a slot behind, and treating that as "already
    /// waiting" would lock the buffer out of ever being read again.
    pub fn install_waiter(&mut self, waiter: NextEditWaiter) -> Result<(), WaitRefusal> {
        if self.waiter.as_ref().is_some_and(|w| !w.is_abandoned()) {
            return Err(WaitRefusal::AlreadyWaiting);
        }
        self.waiter = Some(waiter);
        Ok(())
    }

    /// Removes the waiter **only if it is still the one named**.
    ///
    /// Stated as an identity comparison rather than `waiter = None` so that a
    /// late cleanup for a waiter that has already been replaced cannot
    /// unregister its successor.
    pub fn remove_waiter_if(&mut self, id: WaiterId) {
        if self.waiter.as_ref().is_some_and(|w| w.id() == id) {
            self.waiter = None;
        }
    }

    /// Re-arms at an authoritatively read revision, waking any sleeper.
    pub fn resynchronize(&mut self, revision: Revision) {
        self.sync.resynchronize(revision);
        self.wake_if_ready();
    }

    fn wake_if_ready(&mut self) {
        let has_news = !self.sync.is_synchronized() || self.sync.queued() > 0;
        if !has_news {
            return;
        }
        if let Some(waiter) = self.waiter.take() {
            waiter.wake();
        }
    }
}

impl Drop for BufferSync {
    /// Teardown wakes the sleeper rather than stranding it.
    ///
    /// Dropping the sender closes the channel, so the suspended `next-edit`
    /// resolves with a cancellation instead of parking on a reply that is
    /// never coming — the same guarantee `CommitRequest`'s reply guard makes,
    /// reached the same way: by the type, not by every teardown path
    /// remembering.
    fn drop(&mut self) {
        drop(self.waiter.take());
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

    // ------------------------------------------------ C2: the waiter

    fn waiter(id: u64) -> (NextEditWaiter, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        (NextEditWaiter::new(WaiterId(id), tx), rx)
    }

    #[test]
    fn a_synchronized_empty_buffer_has_nothing_to_say() {
        let mut sync = BufferSync::synchronized(Revision(0));
        assert!(sync.poll(16).is_none(), "and the caller therefore suspends");
    }

    #[test]
    fn an_edit_wakes_the_sleeper() {
        let mut sync = BufferSync::synchronized(Revision(0));
        let (w, mut rx) = waiter(1);
        sync.install_waiter(w).expect("nobody is waiting yet");
        assert!(rx.try_recv().is_err(), "nothing has happened yet");

        sync.record(&edit_of(0, 4));

        assert!(rx.try_recv().is_ok(), "the sleeper was woken");
        assert!(!sync.has_waiter(), "and the slot released");
    }

    /// A collapse is news too. A sleeper that only woke for queued edits would
    /// stay parked through the one event it most needs to hear.
    #[test]
    fn a_collapse_wakes_the_sleeper() {
        let mut sync = BufferSync::synchronized(Revision(0));
        record_n(sync.state_mut_for_test(), MAX_PENDING_EDITS, 1);
        let (w, mut rx) = waiter(1);
        sync.install_waiter(w).expect("nobody is waiting yet");

        sync.record(&edit_of(MAX_PENDING_EDITS as u64, 1));

        assert!(!sync.state().is_synchronized());
        assert!(rx.try_recv().is_ok(), "desynchronization woke the sleeper");
    }

    #[test]
    fn resynchronizing_wakes_a_sleeper_only_when_there_is_news() {
        let mut sync = BufferSync::synchronized(Revision(0));
        record_n(sync.state_mut_for_test(), MAX_PENDING_EDITS + 1, 1);
        let (w, mut rx) = waiter(1);
        sync.install_waiter(w).expect("free");

        sync.resynchronize(Revision(50));

        assert!(
            rx.try_recv().is_err(),
            "re-arming leaves an empty queue, so there is nothing to report \
             and the sleeper stays asleep"
        );
        assert!(sync.has_waiter());
    }

    #[test]
    fn a_second_live_waiter_is_refused() {
        let mut sync = BufferSync::synchronized(Revision(0));
        let (first, _keep) = waiter(1);
        sync.install_waiter(first).expect("free");

        let (second, _rx) = waiter(2);
        assert_eq!(
            sync.install_waiter(second),
            Err(WaitRefusal::AlreadyWaiting),
            "a second reader must fail immediately rather than queue behind \
             the first and race it to serve the same edits"
        );
    }

    /// An abandoned slot must not lock the buffer out forever.
    #[test]
    fn an_abandoned_waiter_is_replaced_rather_than_counted() {
        let mut sync = BufferSync::synchronized(Revision(0));
        let (first, rx) = waiter(1);
        sync.install_waiter(first).expect("free");

        drop(rx); // the guest dropped its next-edit future

        let (second, mut rx2) = waiter(2);
        sync.install_waiter(second)
            .expect("the abandoned slot is evicted, not honoured");

        sync.record(&edit_of(0, 1));
        assert!(
            rx2.try_recv().is_ok(),
            "and the live waiter is the one woken"
        );
    }

    /// Late cleanup for a replaced waiter must not unregister its successor.
    #[test]
    fn removing_an_old_waiter_leaves_its_replacement_alone() {
        let mut sync = BufferSync::synchronized(Revision(0));
        let (first, rx) = waiter(1);
        sync.install_waiter(first).expect("free");
        drop(rx);
        let (second, mut rx2) = waiter(2);
        sync.install_waiter(second)
            .expect("evicts the abandoned one");

        // The first waiter's cleanup arrives late, naming an id that is no
        // longer installed.
        sync.remove_waiter_if(WaiterId(1));

        assert!(sync.has_waiter(), "the replacement survived");
        sync.record(&edit_of(0, 1));
        assert!(rx2.try_recv().is_ok());
    }

    #[test]
    fn removing_the_current_waiter_frees_the_slot() {
        let mut sync = BufferSync::synchronized(Revision(0));
        let (w, _rx) = waiter(7);
        sync.install_waiter(w).expect("free");

        sync.remove_waiter_if(WaiterId(7));

        assert!(!sync.has_waiter());
    }

    /// Dropping the relationship wakes rather than strands.
    #[test]
    fn teardown_resolves_a_suspended_reader() {
        let mut sync = BufferSync::synchronized(Revision(0));
        let (w, mut rx) = waiter(1);
        sync.install_waiter(w).expect("free");

        drop(sync);

        // `try_recv`, never `blocking_recv`. The condition under test is
        // exactly "does this channel close?", so a test that *blocks* on it
        // hangs forever under the mutant it exists to catch — and a suite that
        // hangs is worse than one that fails, because it takes every other
        // test with it and reports nothing. A closed channel answers
        // immediately.
        assert!(
            matches!(rx.try_recv(), Err(oneshot::error::TryRecvError::Closed)),
            "the channel closed, so the suspended call resolves with a \
             cancellation instead of parking on a reply never coming"
        );
    }

    /// Waking hands over no state: the woken caller re-enters and drains.
    #[test]
    fn a_woken_reader_still_has_to_drain() {
        let mut sync = BufferSync::synchronized(Revision(0));
        let (w, mut rx) = waiter(1);
        sync.install_waiter(w).expect("free");
        sync.record(&edit_of(0, 3));
        assert!(rx.try_recv().is_ok());

        let notification = sync.poll(16).expect("the edit is still queued");
        match notification {
            EditNotification::Edits(edits) => assert_eq!(edits.len(), 1),
            other => panic!("expected the queued edit, got {other:?}"),
        }
        assert!(sync.poll(16).is_none(), "and only once");
    }

    #[test]
    fn a_desynchronized_buffer_reports_the_marker_every_time() {
        let mut sync = BufferSync::synchronized(Revision(0));
        record_n(sync.state_mut_for_test(), MAX_PENDING_EDITS + 1, 1);

        for _ in 0..3 {
            assert!(matches!(
                sync.poll(16),
                Some(EditNotification::Desynchronized(_))
            ));
        }
    }

    #[test]
    fn nothing_pending_reports_nothing() {
        let mut state = SyncState::synchronized(Revision(3));
        assert!(state.take_batch(16).is_none());
        assert!(state.is_synchronized());
    }
}
