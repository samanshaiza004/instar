//! Tests whose primary purpose is catching one of the named mutants, where
//! no scenario test in `generation_failures.rs` or `process_failures.rs`
//! already exercises the exact fault. Every other named mutant is verified
//! against an existing scenario test instead -- see the mutant table in the
//! top-level summary for which test proves which mutant, and why re-deriving
//! a second test for the same fault would be redundant rather than
//! additional evidence.

use std::path::{Path, PathBuf};

use recovery_harness::journal::WriteFault;
use recovery_harness::{FakeGuest, RecoveryPolicy, StoreError};

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "recovery-harness-mutant-{}-{}-{}",
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

/// Targets "mark edit durable before write completes".
///
/// A failing journal write must leave `FakeGuest`'s own in-memory document
/// and sequence completely unchanged: no sequence advance, no document
/// mutation, no checkpoint trigger credit. This is the one named mutant no
/// scenario test already exercises, because every scenario test drives real
/// (successful, or realistically torn-by-a-kill) writes -- proving *this*
/// claim needs a write that fails cleanly while the process stays up to
/// inspect the aftermath, which is exactly what `journal::WriteFault` (an
/// in-process fault-injection seam, not a subprocess concern) exists for.
///
/// An earlier version of this test went further and asserted that the
/// `Host` "must still work normally afterward -- a failed write is not a
/// poisoned state." The architectural critique that produced
/// `RecoveryStore`'s poisoning behavior identified that claim as too
/// strong: the *file* changed (it may now hold a torn record) even though
/// nothing in memory did, and treating "the guest is unchanged" as "nothing
/// changed" hid that. This version keeps the original claim about the
/// guest's own state and adds the corrected one: the store is poisoned,
/// and refuses further writes until `repair()` runs.
#[test]
fn a_failed_journal_write_leaves_the_guest_unchanged_but_poisons_the_store() {
    let dir = TestDir::new("failed-write");
    let mut guest = FakeGuest::open(dir.path(), RecoveryPolicy::JOURNAL_EVERY_EDIT_DURABLE)
        .expect("open guest");

    guest
        .apply_edit("first")
        .expect("the first edit succeeds normally");
    let sequence_before = guest.last_presented_sequence();
    let document_before = guest.document().to_string();

    guest
        .journal_writer_mut()
        .set_next_fault(WriteFault::FailBeforeWrite);
    let result = guest.apply_edit("this must not land");

    assert!(
        result.is_err(),
        "a journal write that fails must surface as an error, not a silent success"
    );
    assert_eq!(
        guest.last_presented_sequence(),
        sequence_before,
        "the sequence must not advance past a write that never completed -- this is \
         exactly what 'mark edit durable before write completes' would violate"
    );
    assert_eq!(
        guest.document(),
        document_before,
        "the document must not have absorbed an edit whose journal write failed"
    );
    assert!(
        guest.is_poisoned(),
        "a failed write must poison the store -- the file may hold a torn record \
         even though the guest's own in-memory state did not move"
    );

    // The write genuinely never reached the file: recovering from disk
    // must not show the failed edit either.
    let recovered = FakeGuest::recover_document(dir.path()).expect("recover_document");
    assert_eq!(recovered.content, "first");
    assert_eq!(recovered.last_recovered_sequence, 1);

    // While poisoned, further edits must be refused -- not silently
    // continue as if nothing happened.
    let refused = guest.apply_edit(" second");
    assert!(
        matches!(refused, Err(StoreError::Poisoned)),
        "a poisoned store must refuse further appends until repair() runs"
    );

    // repair() is the documented way out, and after it the guest works
    // normally again.
    guest.repair().expect("repair");
    assert!(!guest.is_poisoned());
    guest
        .apply_edit(" second")
        .expect("apply_edit after repair must succeed");
    assert_eq!(guest.document(), "first second");
    assert_eq!(guest.last_presented_sequence(), 2);
}

/// The same claim, for a write that fails *partway through* rather than
/// before it starts -- the more realistic shape of a real disk-full or
/// I/O-error failure, where some bytes reached the OS before the error.
#[test]
fn a_partially_written_then_failed_journal_append_poisons_the_store_and_is_recoverable_by_repair() {
    let dir = TestDir::new("partial-failed-write");
    let mut guest = FakeGuest::open(dir.path(), RecoveryPolicy::JOURNAL_EVERY_EDIT_DURABLE)
        .expect("open guest");

    guest
        .apply_edit("first")
        .expect("the first edit succeeds normally");
    let sequence_before = guest.last_presented_sequence();

    guest
        .journal_writer_mut()
        .set_next_fault(WriteFault::FailAfterPartialWrite(3));
    let result = guest.apply_edit("this must not land either");

    assert!(result.is_err());
    assert_eq!(guest.last_presented_sequence(), sequence_before);
    assert_eq!(guest.document(), "first");
    assert!(guest.is_poisoned());

    let recovered = FakeGuest::recover_document(dir.path()).expect("recover_document");
    assert_eq!(recovered.content, "first");
    assert!(
        recovered.tail_fault.is_some(),
        "the partial bytes left behind by the failed write must be detected as a \
         torn tail on recovery, not silently ignored"
    );

    guest.repair().expect("repair");
    guest
        .apply_edit(" second")
        .expect("apply_edit after repair must succeed and continue at the right sequence");
    assert_eq!(guest.document(), "first second");
    assert_eq!(guest.last_presented_sequence(), 2);

    let recovered_after_repair = FakeGuest::recover_document(dir.path()).expect("recover_document");
    assert_eq!(recovered_after_repair.content, "first second");
    assert_eq!(recovered_after_repair.tail_fault, None);
}
