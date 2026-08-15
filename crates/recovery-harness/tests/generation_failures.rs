//! Generation-failure scenarios: everything that can go wrong without the
//! host *process* dying. Every fault here is injected in-process, on
//! purpose -- a guest trap, a generation replacement, an input-queue
//! overflow, and a runtime-thread panic are all things the real Instar host
//! process survives by construction (see the document/architecture audit
//! this harness was commissioned by), so simulating them with a real
//! subprocess kill would be proving a stronger claim than the scenario
//! asks for. Real process death is proven separately, in
//! `tests/process_failures.rs`, with a real subprocess.
//!
//! Every test in this file is run against every policy in
//! [`recovery_harness::RecoveryPolicy`]'s named constants, because "what
//! recovery level is expected" is the one thing this harness does not
//! decide -- the assertions below compare recovered state against what
//! *that specific policy* promises, not against a hardcoded expectation.

use std::path::{Path, PathBuf};

use recovery_harness::{Host, InputEvent, RecoveryPolicy, recover};

/// A fresh directory per test/policy combination, cleaned up on drop.
/// Hand-rolled rather than a `tempfile` dependency, matching the rest of
/// this workspace's minimal-dependency posture.
struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "recovery-harness-gen-{}-{}-{}",
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

const ALL_POLICIES: &[(&str, RecoveryPolicy)] = &[
    ("none", RecoveryPolicy::NONE),
    ("durable", RecoveryPolicy::JOURNAL_EVERY_EDIT_DURABLE),
    ("buffered", RecoveryPolicy::JOURNAL_EVERY_EDIT_BUFFERED),
    ("checkpoint-only", RecoveryPolicy::CHECKPOINT_ONLY),
];

/// Whether a policy keeps *any* on-disk record at all -- as opposed to
/// [`RecoveryPolicy::NONE`], which keeps none.
///
/// This is the right predicate for the tests in *this* file, and the wrong
/// one for `tests/process_failures.rs`. Nothing in a generation-failure
/// scenario interrupts a write: the process never dies, so `fsync` versus a
/// bare `write` makes no difference to what a subsequent read sees -- fsync
/// only matters once something can crash between the write and the read,
/// which is exactly the boundary the process-failure tests own. Here, the
/// only question worth asking is whether the policy chose to keep a record
/// in the first place.
fn policy_keeps_any_record(policy: RecoveryPolicy) -> bool {
    !matches!(policy.journal, recovery_harness::JournalMode::Disabled)
        || !matches!(
            policy.checkpoint,
            recovery_harness::CheckpointTrigger::Disabled
        )
}

/// Scenario 1 & 2: a guest trap immediately before, or immediately after,
/// it would have processed a host-committed edit.
///
/// The claim under test: recovery is driven entirely by what the host made
/// durable, never by whether the guest got a chance to consume it. A guest
/// that never lays eyes on an edit before trapping must not lose it from
/// recovery any more than a guest that consumed it a full second earlier.
#[test]
fn guest_trap_timing_does_not_affect_what_recovery_reconstructs() {
    for &(name, policy) in ALL_POLICIES {
        for guest_consumed in [false, true] {
            let dir = TestDir::new(&format!("trap-{name}-{guest_consumed}"));
            let mut host = Host::open(dir.path(), policy).expect("open host");

            let outcome = host.apply_edit("hello").expect("apply_edit");
            if guest_consumed {
                // "Guest traps immediately AFTER processing": the guest
                // incorporated the edit into its own state before dying.
                host.mark_guest_consumed(outcome.sequence);
            }
            // else: "guest traps immediately BEFORE processing" -- the edit
            // reached the host, and the guest never saw it at all.
            let presented = host.last_presented_sequence();
            drop(host); // stands in for the guest generation being gone

            let recovered = recover(
                &Host::journal_path_for(dir.path()),
                &Host::checkpoint_path_for(dir.path()),
            )
            .expect("recover");

            if policy_keeps_any_record(policy) {
                assert_eq!(
                    recovered.content, "hello",
                    "policy {name}: a durably-committed edit must recover regardless \
                     of guest-consumption timing (guest_consumed={guest_consumed})"
                );
                assert_eq!(recovered.last_recovered_sequence, presented);
            }
            assert!(
                recovered.applied_from_journal <= 1,
                "policy {name}: at most one edit exists; recovery must not replay it"
            );
        }
    }

    // The actual cross-check: same policy, same edit, differing only in
    // guest_consumed, produce byte-identical recovered content.
    for &(name, policy) in ALL_POLICIES {
        let dir_before = TestDir::new(&format!("trap-cmp-before-{name}"));
        let mut host = Host::open(dir_before.path(), policy).expect("open host");
        host.apply_edit("hello").expect("apply_edit");
        drop(host);
        let before = recover(
            &Host::journal_path_for(dir_before.path()),
            &Host::checkpoint_path_for(dir_before.path()),
        )
        .expect("recover");

        let dir_after = TestDir::new(&format!("trap-cmp-after-{name}"));
        let mut host = Host::open(dir_after.path(), policy).expect("open host");
        let outcome = host.apply_edit("hello").expect("apply_edit");
        host.mark_guest_consumed(outcome.sequence);
        drop(host);
        let after = recover(
            &Host::journal_path_for(dir_after.path()),
            &Host::checkpoint_path_for(dir_after.path()),
        )
        .expect("recover");

        assert_eq!(
            before, after,
            "policy {name}: recovery must be identical whether the guest trapped \
             before or after it would have consumed the edit"
        );
    }
}

/// Scenario 3: a generation is replaced after an edit lands but before the
/// next UI frame -- the replacement generation must see exactly what the
/// old one committed, no more and no less, and the on-disk state must not
/// grow without bound across the handoff.
#[test]
fn generation_replaced_after_edit_hands_off_exactly_what_was_committed() {
    for &(name, policy) in ALL_POLICIES {
        let dir = TestDir::new(&format!("handoff-{name}"));
        let mut host = Host::open(dir.path(), policy).expect("open host");

        host.apply_edit("hello ").expect("apply_edit");
        host.apply_edit("world").expect("apply_edit");
        let presented_before = host.last_presented_sequence();
        let generation_before = host.generation();

        let handoff = host
            .replace_generation_with_handoff()
            .expect("replace_generation_with_handoff");

        assert_ne!(
            host.generation(),
            generation_before,
            "policy {name}: replacement must actually advance the generation id"
        );

        if policy_keeps_any_record(policy) {
            assert_eq!(
                handoff.content, "hello world",
                "policy {name}: the replacement generation must see exactly what \
                 was committed, byte for byte"
            );
            assert_eq!(handoff.last_recovered_sequence, presented_before);
        }
        // Recovery data bounded: after a handoff, the journal must not
        // still be carrying edits the checkpoint already captured for the
        // *previous* generation forever -- it must have been pruned as
        // part of cleanup, not accumulate across every future generation.
        let journal_len = host.journal_size_bytes().expect("journal_size_bytes");
        assert_eq!(
            journal_len, 0,
            "policy {name}: recovery data must be bounded across a handoff -- the \
             journal must be pruned once its contents are captured, not carried \
             forward forever"
        );
    }
}

/// Scenario 4: the input queue overflows and triggers `InputOverflow`.
///
/// Distinct from every other scenario in this file: overflow is not a
/// failure of the *document* at all, and a policy's `overflow_action` is
/// specifically about whether that distinction is honored -- an overflowing
/// input queue says nothing about whether the document a moment earlier is
/// still trustworthy.
#[test]
fn input_overflow_terminates_the_generation_and_respects_the_overflow_policy() {
    for &(name, policy) in ALL_POLICIES {
        for overflow_action in [
            recovery_harness::OverflowAction::PreserveCheckpoint,
            recovery_harness::OverflowAction::DeleteCheckpoint,
        ] {
            let mut policy = policy;
            policy.overflow_action = overflow_action;
            let dir = TestDir::new(&format!("overflow-{name}-{overflow_action:?}"));
            let mut host = Host::open(dir.path(), policy).expect("open host");

            host.apply_edit("hello").expect("apply_edit");
            host.checkpoint().expect("checkpoint");
            assert!(
                recovery_harness::checkpoint::read_checkpoint_file(&host.checkpoint_path())
                    .expect("read checkpoint")
                    .is_some(),
                "policy {name}: fixture must start with a real checkpoint on disk"
            );

            let generation_before = host.generation();
            let mut overflowed = false;
            for i in 0..policy.input_queue_capacity + 1 {
                if host.push_input(InputEvent(format!("e{i}"))).is_err() {
                    overflowed = true;
                }
            }
            assert!(
                overflowed,
                "policy {name}: the queue must actually overflow"
            );
            assert_ne!(
                host.generation(),
                generation_before,
                "policy {name}: InputOverflow must terminate the generation \
                 (docs/PHASE-3.md's own policy for a queue that cannot take an \
                 ordered event)"
            );

            let checkpoint_survived =
                recovery_harness::checkpoint::read_checkpoint_file(&host.checkpoint_path())
                    .expect("read checkpoint")
                    .is_some();
            match overflow_action {
                recovery_harness::OverflowAction::PreserveCheckpoint => assert!(
                    checkpoint_survived,
                    "policy {name}: PreserveCheckpoint must leave the checkpoint intact"
                ),
                recovery_harness::OverflowAction::DeleteCheckpoint => assert!(
                    !checkpoint_survived,
                    "policy {name}: DeleteCheckpoint must remove the checkpoint"
                ),
            }
        }
    }
}

/// Scenario 5: the runtime thread disappears -- modeled as a real OS thread
/// that panics mid-edit, distinct from the process it runs in, which must
/// keep running. Rust's default `panic = "unwind"` (confirmed: no crate in
/// this workspace sets `panic = "abort"`) means this is a real, observable
/// failure of one thread that the process survives -- not a simulation of
/// one.
#[test]
fn a_panicking_runtime_thread_does_not_take_the_process_or_the_recovery_log_with_it() {
    for &(name, policy) in ALL_POLICIES {
        let dir = TestDir::new(&format!("thread-{name}"));
        let dir_path = dir.path().to_path_buf();
        let policy_owned = policy;

        let handle = std::thread::spawn(move || {
            let mut host = Host::open(&dir_path, policy_owned).expect("open host");
            host.apply_edit("hello").expect("apply_edit");
            panic!("the runtime thread disappearing, on purpose");
        });

        let result = handle.join();
        assert!(
            result.is_err(),
            "policy {name}: the thread must have genuinely panicked, not returned \
             normally -- otherwise this is not testing what it claims to"
        );

        // The main thread -- this test -- is still running, which is the
        // first half of the claim: a runtime-thread failure is not a
        // process failure. The second half: whatever that thread made
        // durable before it died must still be there.
        let recovered = recover(
            &Host::journal_path_for(dir.path()),
            &Host::checkpoint_path_for(dir.path()),
        )
        .expect("recover -- the main thread and the filesystem are both still here");

        if policy_keeps_any_record(policy) {
            assert_eq!(
                recovered.content, "hello",
                "policy {name}: a durable edit must survive its own thread's panic"
            );
        }
    }
}
