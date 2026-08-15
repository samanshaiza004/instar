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

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

use recovery_harness::{Host, recover};

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
    assert!(status.success(), "fixture check: this must be a real clean exit");

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

    let recovered = recover(
        &Host::journal_path_for(dir.path()),
        &Host::checkpoint_path_for(dir.path()),
    )
    .expect("recover after a real kill");

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

    let recovered = recover(
        &Host::journal_path_for(dir.path()),
        &Host::checkpoint_path_for(dir.path()),
    )
    .expect("recover after a clean exit");

    assert_eq!(recovered.content, "hello world");
    assert_eq!(recovered.last_recovered_sequence, 2);
}

/// Scenario 8: the process is killed mid-checkpoint-write. The checkpoint
/// itself is torn and must be refused whole (never partially trusted); the
/// journal entries the checkpoint was *about to* supersede were never
/// pruned (`Host::checkpoint` only prunes after a successful write) and
/// must still be there to fall back on.
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

    let checkpoint_path = Host::checkpoint_path_for(dir.path());
    let torn_checkpoint = recovery_harness::checkpoint::read_checkpoint_file(&checkpoint_path)
        .expect("read the checkpoint file itself");
    assert_eq!(
        torn_checkpoint, None,
        "a torn checkpoint must decode to nothing, never a partial document"
    );

    let recovered = recover(&Host::journal_path_for(dir.path()), &checkpoint_path)
        .expect("recover after a torn checkpoint");
    assert_eq!(
        recovered.content, "seed edit",
        "the journal entry the checkpoint would have superseded is still there, \
         because a checkpoint that never finished writing must not have pruned it"
    );
    assert_eq!(recovered.last_recovered_sequence, 1);
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

    let recovered = recover(
        &Host::journal_path_for(dir.path()),
        &Host::checkpoint_path_for(dir.path()),
    )
    .expect("recover after a torn journal append");

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
        let mut host = Host::open(
            dir.path(),
            recovery_harness::RecoveryPolicy::JOURNAL_EVERY_EDIT_DURABLE,
        )
        .expect("open host");
        host.apply_edit("aaa").expect("apply_edit");
        host.apply_edit("bbb").expect("apply_edit");
        host.apply_edit("ccc").expect("apply_edit");
        drop(host);

        let journal_path = Host::journal_path_for(dir.path());
        let mut bytes = std::fs::read(&journal_path).expect("read journal");
        // Flip one byte inside the payload of the second record. Each
        // record here is FIXED_OVERHEAD(20) + payload_len(3) = 23 bytes;
        // the second record starts at byte 23, and its payload starts 16
        // bytes into that record.
        let second_record_start = 23;
        let flip_at = second_record_start + 16 + 1;
        bytes[flip_at] ^= 0xFF;
        std::fs::write(&journal_path, &bytes).expect("write corrupted journal");

        let checkpoint_path = Host::checkpoint_path_for(dir.path());
        let first = recover(&journal_path, &checkpoint_path).expect("recover (first pass)");
        let second = recover(&journal_path, &checkpoint_path).expect("recover (second pass)");

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
        let mut host = Host::open(
            dir.path(),
            recovery_harness::RecoveryPolicy::JOURNAL_EVERY_EDIT_DURABLE,
        )
        .expect("open host");
        host.apply_edit("hello").expect("apply_edit");
        drop(host);

        let journal_path = Host::journal_path_for(dir.path());
        recovery_harness::checkpoint::write_raw(&journal_path, &[0xDE, 0xAD, 0xBE])
            .expect("truncate journal to 3 garbage bytes");

        let checkpoint_path = Host::checkpoint_path_for(dir.path());
        let recovered = recover(&journal_path, &checkpoint_path)
            .expect("recover must not error, even on a file this short");
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
        let journal_path = Host::journal_path_for(dir.path());
        std::fs::write(&journal_path, []).expect("write empty journal");
        let checkpoint_path = Host::checkpoint_path_for(dir.path());
        let recovered = recover(&journal_path, &checkpoint_path).expect("recover an empty journal");
        assert_eq!(recovered.content, "");
        assert_eq!(recovered.tail_fault, None, "empty is healthy, not corrupt");
    }
}
