//! Process-failure scenarios: everything that requires the host *process*
//! to actually die. Every kill in this file is a real `SIGKILL` /
//! `TerminateProcess` sent to a real, separate child process spawned with
//! `std::process::Command` -- never an in-process object dropped and called
//! proof of a crash. `assert_was_really_killed` exists specifically to make
//! that distinction load-bearing rather than a comment: it checks the
//! child's actual exit status for evidence of a forceful kill, which is
//! also this file's answer to the "process-crash test never actually kills
//! the process" named mutant.
//!
//! Scenario 10 (corrupted/truncated recovery record) is the one exception
//! to "every process-failure scenario spawns a subprocess": corruption is a
//! property of bytes on disk, reachable by many causes (bit rot, a crash
//! this file's own scenarios 8/9 already prove, a bad disk sector), and
//! this file tests the *reader's* response to that state directly rather
//! than re-deriving it from a fresh kill every time.
//!
//! Two scenarios beyond the original ten were added by the architectural
//! critique this file's `RecoveryStore`/`FakeGuest` split responds to:
//! a real crash mid-write to a *second* checkpoint, proving it cannot
//! destroy a first checkpoint that was already durable (the concrete
//! failure the old in-place-write mechanism could not rule out), and a
//! multi-cycle "recover, continue, crash again" run, proving the property
//! holds repeatedly rather than only once.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

use recovery_harness::FakeGuest;

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "recovery-harness-proc-{}-{}-{}",
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

fn fault_child_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fault_child")
}

/// Spawns a real `fault_child` subprocess with a piped stdout, so the
/// parent can synchronize on its `READY` line rather than guess at timing.
fn spawn(dir: &Path, policy: &str, mode_args: &[&str]) -> Child {
    let mut args: Vec<&str> = vec![dir.to_str().expect("utf8 path"), policy];
    args.extend_from_slice(mode_args);
    Command::new(fault_child_bin())
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fault_child")
}

/// Blocks until the child prints its `READY` line -- a deterministic
/// handshake, not a timed guess about when the child's write finished.
fn wait_for_ready(child: &mut Child) {
    let stdout = child.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read READY line");
    assert_eq!(
        line.trim(),
        "READY",
        "fault_child did not reach its ready point before exiting or hanging \
         some other way; stderr may have more detail"
    );
}

/// Sends a real `SIGKILL` (Unix) / `TerminateProcess` (Windows) and reaps
/// the process, returning its exit status as evidence the kill was real.
fn kill_and_reap(child: &mut Child) -> ExitStatus {
    child.kill().expect("kill (SIGKILL / TerminateProcess)");
    child.wait().expect("reap the killed process")
}

/// The counter-test for the "process-crash test never actually kills the
/// process" mutant: a status that looks like a clean exit here means the
/// kill never happened, regardless of what the recovered document looks
/// like afterward.
#[cfg(unix)]
fn assert_was_really_killed(status: &ExitStatus) {
    use std::os::unix::process::ExitStatusExt;
    assert!(
        !status.success(),
        "a really-killed process must not report success"
    );
    assert_eq!(
        status.signal(),
        Some(9),
        "the child must have been terminated by SIGKILL specifically -- any other \
         status means the 'kill' in this test did not really happen"
    );
}

#[cfg(not(unix))]
fn assert_was_really_killed(status: &ExitStatus) {
    assert!(
        !status.success(),
        "a really-killed process must not report success"
    );
}

/// The "process-crash test never actually kills the process" mutant, made
/// concrete: this is what `assert_was_really_killed` exists to catch in
/// scenarios 6, 8, and 9, proven directly rather than trusted by
/// inspection. A status this harness produces without ever calling `kill`
/// -- a real, clean exit -- must be rejected by the same function every
/// kill-based scenario relies on to know its kill was real.
#[test]
fn assert_was_really_killed_rejects_a_status_that_was_never_killed() {
    let dir = TestDir::new("meta-not-really-killed");
    let mut child = spawn(dir.path(), "durable", &["exit-after-edits", "hello"]);
    wait_for_ready(&mut child);
    // Deliberately no `.kill()` call: this status is a real, unmutated
    // clean exit, standing in for what a broken "kill" scenario -- one
    // that silently stopped killing anything -- would produce instead.
    let status = child.wait().expect("wait for the clean exit");
    assert!(
        status.success(),
        "fixture check: this must be a real clean exit"
    );

    let caught = std::panic::catch_unwind(|| assert_was_really_killed(&status));
    assert!(
        caught.is_err(),
        "assert_was_really_killed must reject a clean-exit status -- if it does \
         not, a scenario whose kill silently stopped happening would still pass"
    );
}

/// Scenario 6: the host process is forcibly killed, with nothing in flight
/// -- the baseline "a hard kill loses nothing that was already durable"
/// case, contrasted against scenarios 8 and 9 where something *is* in
/// flight.
#[test]
fn host_process_forcibly_killed_with_nothing_in_flight() {
    let dir = TestDir::new("hard-kill");
    let mut child = spawn(
        dir.path(),
        "durable",
        &["block-after-edits", "hello", " world"],
    );
    wait_for_ready(&mut child);
    let status = kill_and_reap(&mut child);
    assert_was_really_killed(&status);

    let recovered = FakeGuest::recover_document(dir.path()).expect("recover_document after a real kill");

    assert_eq!(
        recovered.content, "hello world",
        "a hard kill with nothing in flight must lose nothing that was \
         already durable"
    );
    assert_eq!(recovered.last_recovered_sequence, 2);
    assert_eq!(
        recovered.tail_fault, None,
        "nothing was in flight to corrupt"
    );
}

/// Scenario 7: a clean application restart. Included specifically to make
/// the audit's own finding checkable: a clean exit is not, by itself, any
/// more durable than a kill -- both recover exactly what the policy already
/// made durable, no more.
#[test]
fn clean_restart_recovers_exactly_what_was_durable_no_more_no_less() {
    let dir = TestDir::new("clean-restart");
    let mut child = spawn(
        dir.path(),
        "durable",
        &["exit-after-edits", "hello", " world"],
    );
    wait_for_ready(&mut child);
    let status = child.wait().expect("wait for clean exit");
    assert!(
        status.success(),
        "the child must have exited cleanly, not crashed"
    );

    let recovered = FakeGuest::recover_document(dir.path()).expect("recover_document after a clean exit");

    assert_eq!(recovered.content, "hello world");
    assert_eq!(recovered.last_recovered_sequence, 2);
}

/// Scenario 8: the process is killed mid-checkpoint-write. The checkpoint
/// itself is torn and must be refused whole (never partially trusted); the
/// journal entries the checkpoint was *about to* supersede were never
/// pruned (`RecoveryStore::write_checkpoint` only prunes after a successful
/// write) and must still be there to fall back on.
#[test]
fn killed_during_checkpoint_write_falls_back_to_the_untouched_journal() {
    let dir = TestDir::new("kill-mid-checkpoint");
    // A checkpoint record for "seed edit" is comfortably more than 20 bytes
    // once encoded (4 magic + 8 sequence + 4 length + content + 4 checksum),
    // so 20 guarantees a torn write, not a coincidentally-complete one.
    let mut child = spawn(
        dir.path(),
        "durable",
        &["block-mid-checkpoint-write", "20", "seed edit"],
    );
    wait_for_ready(&mut child);
    let status = kill_and_reap(&mut child);
    assert_was_really_killed(&status);

    let checkpoint_path = FakeGuest::checkpoint_path_for(dir.path());
    let torn_checkpoint = recovery_harness::checkpoint::read_checkpoint_file(&checkpoint_path)
        .expect("read the checkpoint file itself");
    assert_eq!(
        torn_checkpoint, None,
        "a torn checkpoint must decode to nothing, never a partial document"
    );

    let recovered = FakeGuest::recover_document(dir.path()).expect("recover_document after a torn checkpoint");
    assert_eq!(
        recovered.content, "seed edit",
        "the journal entry the checkpoint would have superseded is still there, \
         because a checkpoint that never finished writing must not have pruned it"
    );
    assert_eq!(recovered.last_recovered_sequence, 1);
}

/// New scenario: the process is killed mid-write to a *second* checkpoint,
/// with a first checkpoint already durably installed. This is the exact
/// production-shaped scenario the architectural critique identified as
/// unproven by the old in-place `truncate(true)`-then-write mechanism: that
/// mechanism truncated the existing checkpoint file before a single byte
/// of the replacement had landed, so a crash in that window could destroy
/// the *only* durable copy of the document, not merely leave a torn new
/// one. The write-tmp/fsync/rename/fsync-parent-dir mechanism in
/// `checkpoint::write_checkpoint_atomic` never touches `checkpoint.bin`
/// until the rename, which is atomic -- this test is what would have been
/// red under the old mechanism and is green under the new one.
#[test]
fn a_crash_during_a_second_checkpoint_write_leaves_the_first_checkpoint_intact() {
    let dir = TestDir::new("checkpoint-b-preserves-checkpoint-a");

    let mut setup = spawn(dir.path(), "durable", &["establish-checkpoint", "checkpoint A content"]);
    wait_for_ready(&mut setup);
    let status = setup.wait().expect("wait for clean exit establishing checkpoint A");
    assert!(status.success());

    let checkpoint_a =
        recovery_harness::checkpoint::read_checkpoint_file(&FakeGuest::checkpoint_path_for(dir.path()))
            .expect("read checkpoint A")
            .expect("checkpoint A must exist after establish-checkpoint");

    // Resume against the same directory (picking up checkpoint A) and get
    // killed while writing checkpoint B.
    let mut child = spawn(
        dir.path(),
        "durable",
        &[
            "block-mid-checkpoint-write",
            "20",
            " plus enough more content to make checkpoint B",
        ],
    );
    wait_for_ready(&mut child);
    let status = kill_and_reap(&mut child);
    assert_was_really_killed(&status);

    let checkpoint_after =
        recovery_harness::checkpoint::read_checkpoint_file(&FakeGuest::checkpoint_path_for(dir.path()))
            .expect("read checkpoint after the crash");
    assert_eq!(
        checkpoint_after,
        Some(checkpoint_a),
        "a crash mid-write to the *next* checkpoint must never destroy the \
         previous, already-durable checkpoint"
    );
}

/// Scenario 9: the process is killed mid-journal-append. Everything durably
/// appended before the fatal write must recover; the interrupted record
/// must not appear at all, whole or partial -- and the reader must be able
/// to say exactly where it stopped trusting the file.
#[test]
fn killed_during_journal_append_loses_only_the_interrupted_record() {
    let dir = TestDir::new("kill-mid-journal");
    let mut child = spawn(
        dir.path(),
        "durable",
        &[
            "block-mid-journal-write",
            "10",
            "one",
            "two",
            "--",
            "unfinished",
        ],
    );
    wait_for_ready(&mut child);
    let status = kill_and_reap(&mut child);
    assert_was_really_killed(&status);

    let recovered = FakeGuest::recover_document(dir.path()).expect("recover_document after a torn journal append");

    assert_eq!(
        recovered.content, "onetwo",
        "duplicate edit did not replay twice, and the interrupted third edit \
         must not appear at all -- not whole, not partial"
    );
    assert_eq!(
        recovered.last_recovered_sequence, 2,
        "last acknowledged recovery sequence stops at the last complete record"
    );
    assert_eq!(recovered.applied_from_journal, 2);
    assert!(
        recovered.tail_fault.is_some(),
        "the reader must report that it found and stopped at a torn record, \
         not silently recover a truncated file as if it were complete"
    );
}

/// New scenario, the direct realization of the architectural critique's
/// "multi-crash 'recover, continue, crash again'" request: single
/// failure-then-inspect tests cannot see a bug where a resumed writer
/// starts from the wrong sequence and either loses or duplicates data on
/// the *next* crash. Three real kills in a row against the same directory,
/// each one resuming from where the last left off via `FakeGuest::resume`.
#[test]
fn recovering_then_continuing_then_crashing_again_does_not_lose_or_duplicate_anything() {
    let dir = TestDir::new("multi-cycle");

    let mut child1 = spawn(dir.path(), "durable", &["block-after-edits", "one", "two"]);
    wait_for_ready(&mut child1);
    let status1 = kill_and_reap(&mut child1);
    assert_was_really_killed(&status1);

    let after_cycle_1 = FakeGuest::recover_document(dir.path()).expect("recover after cycle 1");
    assert_eq!(after_cycle_1.content, "onetwo");
    assert_eq!(after_cycle_1.last_recovered_sequence, 2);
    assert_eq!(after_cycle_1.sequence_gap, None);

    // Cycle 2: a fresh process resumes the same directory and continues --
    // its edit's sequence must pick up from 3, not restart at 1 and collide
    // with what cycle 1 already durably wrote.
    let mut child2 = spawn(dir.path(), "durable", &["block-after-edits", "three"]);
    wait_for_ready(&mut child2);
    let status2 = kill_and_reap(&mut child2);
    assert_was_really_killed(&status2);

    let after_cycle_2 = FakeGuest::recover_document(dir.path()).expect("recover after cycle 2");
    assert_eq!(
        after_cycle_2.content, "onetwothree",
        "continuing after a recovery must not lose cycle 1's edits nor duplicate them"
    );
    assert_eq!(after_cycle_2.last_recovered_sequence, 3);
    assert_eq!(
        after_cycle_2.sequence_gap, None,
        "sequences must stay contiguous across a recover-then-continue cycle"
    );

    // Cycle 3: crash again, proving the property holds repeatedly, not
    // just once.
    let mut child3 = spawn(dir.path(), "durable", &["block-after-edits", "four"]);
    wait_for_ready(&mut child3);
    let status3 = kill_and_reap(&mut child3);
    assert_was_really_killed(&status3);

    let after_cycle_3 = FakeGuest::recover_document(dir.path()).expect("recover after cycle 3");
    assert_eq!(after_cycle_3.content, "onetwothreefour");
    assert_eq!(after_cycle_3.last_recovered_sequence, 4);
    assert_eq!(after_cycle_3.sequence_gap, None);
}

/// Scenario 10: a corrupted or truncated recovery record, examined directly
/// rather than freshly produced by a kill -- corruption is a property of
/// bytes, reachable by more causes than a crash mid-write (scenarios 8 and
/// 9 already prove that cause specifically).
#[test]
fn corrupted_or_truncated_recovery_record_is_handled_deterministically() {
    // Case A: a bit flip in the *middle* record of three, surrounded by
    // valid records on both sides. The reader must keep everything before
    // the flip and discard everything from it onward, even though the
    // third record, taken alone, would still be well-formed.
    {
        let dir = TestDir::new("corrupt-mid-record");
        let mut guest = FakeGuest::open(
            dir.path(),
            recovery_harness::RecoveryPolicy::JOURNAL_EVERY_EDIT_DURABLE,
        )
        .expect("open guest");
        guest.apply_edit("aaa").expect("apply_edit");
        guest.apply_edit("bbb").expect("apply_edit");
        guest.apply_edit("ccc").expect("apply_edit");
        drop(guest);

        let journal_path = FakeGuest::journal_path_for(dir.path());
        let mut bytes = std::fs::read(&journal_path).expect("read journal");
        // Flip one byte inside the payload of the second record. Each
        // record here is FIXED_OVERHEAD(20) + payload_len(3) = 23 bytes;
        // the second record starts at byte 23, and its payload starts 16
        // bytes into that record.
        let second_record_start = 23;
        let flip_at = second_record_start + 16 + 1;
        bytes[flip_at] ^= 0xFF;
        std::fs::write(&journal_path, &bytes).expect("write corrupted journal");

        let first = FakeGuest::recover_document(dir.path()).expect("recover_document (first pass)");
        let second = FakeGuest::recover_document(dir.path()).expect("recover_document (second pass)");

        assert_eq!(
            first, second,
            "corrupt tail handled deterministically: the same corrupt bytes must \
             recover identically on repeated reads"
        );
        assert_eq!(
            first.content, "aaa",
            "only the record before the corruption is trustworthy, even though a \
             well-formed third record follows it in the file"
        );
        assert_eq!(first.last_recovered_sequence, 1);
        assert_eq!(first.applied_from_journal, 1);
        assert!(first.tail_fault.is_some());
    }

    // Case B: the file truncated to a handful of bytes -- fewer than even
    // one record's fixed header. Must not panic, must recover nothing.
    {
        let dir = TestDir::new("corrupt-header-truncated");
        let mut guest = FakeGuest::open(
            dir.path(),
            recovery_harness::RecoveryPolicy::JOURNAL_EVERY_EDIT_DURABLE,
        )
        .expect("open guest");
        guest.apply_edit("hello").expect("apply_edit");
        drop(guest);

        let journal_path = FakeGuest::journal_path_for(dir.path());
        recovery_harness::checkpoint::write_raw(&journal_path, &[0xDE, 0xAD, 0xBE])
            .expect("truncate journal to 3 garbage bytes");

        let recovered = FakeGuest::recover_document(dir.path())
            .expect("recover_document must not error, even on a file this short");
        assert_eq!(recovered.content, "");
        assert_eq!(recovered.last_recovered_sequence, 0);
        assert_eq!(recovered.applied_from_journal, 0);
        assert_eq!(
            recovered.tail_fault,
            Some(recovery_harness::TailFault::HeaderTruncated)
        );
    }

    // Case C: an empty file -- distinct from Case B, and the one case that
    // must report no fault at all (an empty journal is not a torn one).
    {
        let dir = TestDir::new("corrupt-empty-file");
        let journal_path = FakeGuest::journal_path_for(dir.path());
        std::fs::write(&journal_path, []).expect("write empty journal");
        let recovered = FakeGuest::recover_document(dir.path()).expect("recover_document an empty journal");
        assert_eq!(recovered.content, "");
        assert_eq!(recovered.tail_fault, None, "empty is healthy, not corrupt");
    }
}
