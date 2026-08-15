//! Tests whose primary purpose is catching one of the seven named mutants,
//! where no scenario test in `generation_failures.rs` or
//! `process_failures.rs` already exercises the exact fault. Every other
//! named mutant is verified against an existing scenario test instead --
//! see the mutant table in the top-level summary for which test proves
//! which mutant, and why re-deriving a second test for the same fault would
//! be redundant rather than additional evidence.

use std::path::{Path, PathBuf};

use recovery_harness::journal::WriteFault;
use recovery_harness::{Host, RecoveryPolicy};

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
/// A failing journal write must leave `Host` completely unchanged: no
/// sequence advance, no document mutation, no checkpoint trigger credit.
/// This is the one named mutant no scenario test already exercises, because
/// every scenario test drives real (successful, or realistically torn-by-a-
/// kill) writes -- proving *this* claim needs a write that fails cleanly
/// while the process stays up to inspect the aftermath, which is exactly
/// what `journal::WriteFault` (an in-process fault-injection seam, not a
/// subprocess concern) exists for.
#[test]
fn a_failed_journal_write_leaves_the_host_completely_unchanged() {
    let dir = TestDir::new("failed-write");
    let mut host =
        Host::open(dir.path(), RecoveryPolicy::JOURNAL_EVERY_EDIT_DURABLE).expect("open host");

    host.apply_edit("first").expect("the first edit succeeds normally");
    let sequence_before = host.last_presented_sequence();
    let document_before = host.document().to_string();

    host.journal_writer_mut()
        .set_next_fault(WriteFault::FailBeforeWrite);
    let result = host.apply_edit("this must not land");

    assert!(
        result.is_err(),
        "a journal write that fails must surface as an error, not a silent success"
    );
    assert_eq!(
        host.last_presented_sequence(),
        sequence_before,
        "the sequence must not advance past a write that never completed -- this is \
         exactly what 'mark edit durable before write completes' would violate"
    );
    assert_eq!(
        host.document(),
        document_before,
        "the document must not have absorbed an edit whose journal write failed"
    );

    // The write genuinely never reached the file: recovering from disk
    // must not show the failed edit either.
    let recovered = recovery_harness::recover(&host.journal_path(), &host.checkpoint_path())
        .expect("recover");
    assert_eq!(recovered.content, "first");
    assert_eq!(recovered.last_recovered_sequence, 1);

    // The host must still work normally afterward -- a failed write is not
    // a poisoned state.
    host.apply_edit(" second").expect("apply_edit after a prior failure");
    assert_eq!(host.document(), "first second");
    assert_eq!(host.last_presented_sequence(), 2);
}

/// The same claim, for a write that fails *partway through* rather than
/// before it starts -- the more realistic shape of a real disk-full or
/// I/O-error failure, where some bytes reached the OS before the error.
#[test]
fn a_partially_written_then_failed_journal_append_leaves_the_host_unchanged() {
    let dir = TestDir::new("partial-failed-write");
    let mut host =
        Host::open(dir.path(), RecoveryPolicy::JOURNAL_EVERY_EDIT_DURABLE).expect("open host");

    host.apply_edit("first").expect("the first edit succeeds normally");
    let sequence_before = host.last_presented_sequence();

    host.journal_writer_mut()
        .set_next_fault(WriteFault::FailAfterPartialWrite(3));
    let result = host.apply_edit("this must not land either");

    assert!(result.is_err());
    assert_eq!(host.last_presented_sequence(), sequence_before);
    assert_eq!(host.document(), "first");

    let recovered = recovery_harness::recover(&host.journal_path(), &host.checkpoint_path())
        .expect("recover");
    assert_eq!(recovered.content, "first");
    assert!(
        recovered.tail_fault.is_some(),
        "the partial bytes left behind by the failed write must be detected as a \
         torn tail on recovery, not silently ignored"
    );
}
