//! `RecoveryStore` mechanism tests that don't fit the generation/process
//! failure split cleanly, because they exercise the store directly rather
//! than through a failure scenario: torn-tail detection on open, the
//! poisoned-until-repaired gate, and append-time sequence-continuity
//! enforcement. Added by the architectural critique that split
//! `RecoveryStore` out of the old `Host` -- these are properties of the
//! opaque mechanism itself, provable without any document semantics at all.

use std::path::{Path, PathBuf};

use recovery_harness::{
    FakeGuest, RecoveryPolicy, RecoveryStore, SequenceGap, StoreBounds, StoreError,
};

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "recovery-harness-store-{}-{}-{}",
            std::process::id(),
            label,
            n
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn corrupt_tail(journal_path: &Path) {
    let mut bytes = std::fs::read(journal_path).expect("read journal");
    let cut = bytes.len().saturating_sub(2);
    bytes.truncate(cut);
    std::fs::write(journal_path, &bytes).expect("write torn journal");
}

/// A torn tail left by a previous process (standing in for one that died
/// mid-append) must poison the store the moment it is opened, before any
/// caller does anything with it -- not only after a fresh in-process
/// failure.
#[test]
fn a_pre_existing_torn_tail_poisons_the_store_on_open() {
    let dir = TestDir::new("torn-on-open");
    {
        let mut store = RecoveryStore::open(dir.path(), StoreBounds::DEFAULT).expect("open store");
        store.append(1, b"a", true).expect("append a");
        store.append(2, b"b", true).expect("append b");
    }

    corrupt_tail(&RecoveryStore::journal_path_for(dir.path()));

    let store = RecoveryStore::open(dir.path(), StoreBounds::DEFAULT).expect("re-open store");
    assert!(
        store.is_poisoned(),
        "a store opened against a torn tail must start poisoned"
    );
}

/// A poisoned store refuses every append until `repair()` runs; `repair()`
/// truncates to the trusted tail and appends afterward continue correctly
/// from there.
#[test]
fn a_poisoned_store_refuses_appends_until_repaired_then_continues_correctly() {
    let dir = TestDir::new("poisoned-refuses");
    {
        let mut store = RecoveryStore::open(dir.path(), StoreBounds::DEFAULT).expect("open store");
        // Two complete records first, so the corruption below tears only
        // the second one and leaves a genuine trusted prefix to repair
        // back to -- corrupting a file holding just one record would
        // destroy that record entirely rather than leaving one intact.
        store.append(1, b"a", true).expect("append a");
        store.append(2, b"b", true).expect("append b");
    }
    corrupt_tail(&RecoveryStore::journal_path_for(dir.path()));

    let mut store = RecoveryStore::open(dir.path(), StoreBounds::DEFAULT).expect("re-open store");
    assert!(store.is_poisoned());

    let refused = store.append(3, b"c", true);
    assert!(
        matches!(refused, Err(StoreError::Poisoned)),
        "a poisoned store must refuse an append rather than write beside a torn tail"
    );

    store.repair().expect("repair");
    assert!(!store.is_poisoned());

    // Record 2 ("b") was the one torn away, so the correct resume point is
    // sequence 2 again, not 3 -- repair must have rolled the store's own
    // notion of "last sequence" back to what actually survived, not left
    // it pointing past the record repair just discarded.
    let still_three = store.append(3, b"c", true);
    assert!(
        matches!(
            still_three,
            Err(StoreError::SequenceMismatch {
                expected: 2,
                found: 3
            })
        ),
        "repair must roll the resume point back to the trusted tail, not leave it \
         pointing past the record that was just discarded: {still_three:?}"
    );
    store
        .append(2, b"b-retry", true)
        .expect("re-appending the lost sequence must succeed after repair");

    let read = store.read().expect("read");
    assert_eq!(
        read.journal_records.len(),
        2,
        "the trusted first record must survive repair; the torn second record must be gone entirely"
    );
    assert_eq!(read.journal_records[0].sequence, 1);
    assert_eq!(read.journal_records[1].sequence, 2);
    assert_eq!(read.journal_records[1].payload, b"b-retry");
    assert_eq!(read.sequence_gap, None);
}

/// `RecoveryStore::append` refuses a non-contiguous sequence outright,
/// before it ever reaches the journal writer -- a pure ordering check that
/// needs no interpretation of the payload.
#[test]
fn append_refuses_a_non_contiguous_sequence() {
    let dir = TestDir::new("append-sequence-check");
    let mut store = RecoveryStore::open(dir.path(), StoreBounds::DEFAULT).expect("open store");
    store.append(1, b"a", true).expect("append 1");

    let skipped = store.append(3, b"c", true);
    assert!(
        matches!(
            skipped,
            Err(StoreError::SequenceMismatch {
                expected: 2,
                found: 3
            })
        ),
        "append must refuse a sequence that skips ahead: {skipped:?}"
    );

    let duplicate = store.append(1, b"a-again", true);
    assert!(
        matches!(
            duplicate,
            Err(StoreError::SequenceMismatch {
                expected: 2,
                found: 1
            })
        ),
        "append must refuse a sequence that repeats one already recorded: {duplicate:?}"
    );

    // The store must still be healthy after refusing both -- a refusal is
    // not itself a fault.
    assert!(!store.is_poisoned());
    store
        .append(2, b"b", true)
        .expect("the correct next sequence still succeeds");
}

/// A journal whose sequence numbers are not contiguous with the checkpoint
/// (constructed here directly, since `RecoveryStore::append` on its own
/// can never produce this shape) is reported as a gap at the exact point
/// it occurs, and replay stops there rather than skipping over it.
#[test]
fn a_sequence_gap_stops_replay_at_the_gap_not_after_it() {
    let dir = TestDir::new("sequence-gap");
    {
        let mut store = RecoveryStore::open(dir.path(), StoreBounds::DEFAULT).expect("open store");
        store.append(1, b"a", true).expect("append 1");
    }
    // Hand-construct a gap: sequence 2 is simply never written, standing in
    // for a record that was acknowledged but never made durable under a
    // weaker-than-durable policy. `RecoveryStore::append`'s own contiguity
    // check cannot produce this on its own, which is exactly why this test
    // builds the file by hand rather than through the store.
    {
        let mut store =
            RecoveryStore::open(dir.path(), StoreBounds::DEFAULT).expect("re-open store");
        // Bypass the store's own contiguity check to construct the gap.
        store
            .journal_writer_mut()
            .append(3, b"c", true)
            .expect("append 3 directly, skipping 2");
    }

    let recovered = FakeGuest::recover_document(dir.path()).expect("recover_document");
    assert_eq!(recovered.content, "a");
    assert_eq!(recovered.last_recovered_sequence, 1);
    assert_eq!(
        recovered.sequence_gap,
        Some(SequenceGap {
            expected: 2,
            found: 3
        })
    );
}

/// A failed checkpoint write (the tmp-file write itself faulted) must not
/// poison the store: by construction it never touched the journal or the
/// previous checkpoint, so there is nothing torn to repair.
#[test]
fn a_failed_checkpoint_write_does_not_poison_the_store() {
    let dir = TestDir::new("failed-checkpoint-no-poison");
    let mut store = RecoveryStore::open(dir.path(), StoreBounds::DEFAULT).expect("open store");
    store.append(1, b"a", true).expect("append 1");

    let result = store.write_checkpoint_faulted(
        1,
        b"a",
        recovery_harness::checkpoint::WriteFault::FailAfterPartialWrite(2),
    );
    assert!(result.is_err());
    assert!(
        !store.is_poisoned(),
        "a checkpoint write that never touched the journal or the prior \
         checkpoint must not poison the store"
    );

    // The journal is untouched -- a failed checkpoint must not have pruned
    // anything it never durably superseded.
    let read = store.read().expect("read");
    assert_eq!(read.journal_records.len(), 1);
    assert_eq!(read.checkpoint, None);

    // The store still works normally afterward.
    store
        .append(2, b"b", true)
        .expect("append 2 after a failed checkpoint");
}

/// Resuming a guest against an existing, healthy scope directory picks up
/// exactly where the previous one left off, at the right document and the
/// right sequence -- the property the multi-cycle process-failure tests in
/// `tests/process_failures.rs` depend on.
#[test]
fn resume_picks_up_the_document_and_sequence_a_previous_guest_left_behind() {
    let dir = TestDir::new("resume");
    {
        let mut guest = FakeGuest::open(dir.path(), RecoveryPolicy::JOURNAL_EVERY_EDIT_DURABLE)
            .expect("open guest");
        guest.apply_edit("hello").expect("apply_edit");
        guest.apply_edit(" world").expect("apply_edit");
    }

    let mut resumed = FakeGuest::resume(dir.path(), RecoveryPolicy::JOURNAL_EVERY_EDIT_DURABLE)
        .expect("resume guest");
    assert_eq!(resumed.document(), "hello world");
    assert_eq!(resumed.last_presented_sequence(), 2);

    resumed
        .apply_edit("!")
        .expect("continue editing after resume");
    assert_eq!(resumed.document(), "hello world!");
    assert_eq!(resumed.last_presented_sequence(), 3);

    let recovered = FakeGuest::recover_document(dir.path()).expect("recover_document");
    assert_eq!(recovered.content, "hello world!");
    assert_eq!(recovered.last_recovered_sequence, 3);
}
