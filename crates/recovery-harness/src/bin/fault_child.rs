//! The real subprocess the process-failure tests spawn and kill.
//!
//! Never invoked directly by a person. Every mode below ends one of two
//! ways: a clean `exit(0)` (the "clean restart" scenario), or printing a
//! `READY` line to stdout and then blocking forever, waiting for the parent
//! test to have sent a real `SIGKILL` / `TerminateProcess` by the time it
//! gives up waiting. The blocking modes never exit on their own; if one is
//! ever observed to exit, that is a bug in the parent test's kill logic, not
//! in this binary.
//!
//! # Why the "mid-write" modes write a *deliberately* incomplete record
//! rather than racing a real write against a timed kill
//!
//! A timed kill racing a real in-flight `write_all` would be a real crash in
//! the most literal sense, but it would also be flaky: whether the kill
//! lands before or after the OS finishes the write depends on scheduling
//! this test does not control. Instead, this binary writes a *chosen* number
//! of bytes of the next record, `fsync`s exactly those bytes, and then
//! blocks -- so the on-disk file is left in a state a real crash mid-write
//! could have produced, deterministically, and the subsequent `SIGKILL`
//! proves the *process* really died (not an in-process object being
//! dropped) rather than manufacturing the file corruption itself. The two
//! concerns -- "is the file torn the way a crash would tear it" and "did a
//! real process really die" -- are deliberately separated and each proven
//! for real.

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use recovery_harness::{Host, RecoveryPolicy};

fn print_ready() {
    println!("READY");
    io::stdout().flush().expect("stdout flush");
}

fn block_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: fault_child <dir> <policy> <mode> [args...]\n\
         policy: none | durable | buffered | checkpoint-only\n\
         modes:\n\
         \x20 exit-after-edits <edit>...\n\
         \x20 block-after-edits <edit>...\n\
         \x20 block-mid-journal-write <n_bytes> <edit>... -- <final_edit>\n\
         \x20 block-mid-checkpoint-write <n_bytes> <edit>..."
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [dir, policy_name, mode, rest @ ..] = args.as_slice() else {
        usage();
    };
    let dir = PathBuf::from(dir);
    let policy = RecoveryPolicy::by_name(policy_name).unwrap_or_else(|| {
        eprintln!("unknown policy: {policy_name}");
        std::process::exit(2);
    });

    let mut host = Host::open(&dir, policy).expect("open host");

    match mode.as_str() {
        "exit-after-edits" => {
            for edit in rest {
                host.apply_edit(edit.clone()).expect("apply_edit");
            }
            print_ready();
            std::process::exit(0);
        }
        "block-after-edits" => {
            for edit in rest {
                host.apply_edit(edit.clone()).expect("apply_edit");
            }
            print_ready();
            block_forever();
        }
        "block-mid-journal-write" => {
            let Some((n_bytes_arg, rest)) = rest.split_first() else {
                usage();
            };
            let n_bytes: usize = n_bytes_arg.parse().expect("n_bytes is a number");
            let separator = rest.iter().position(|a| a == "--").unwrap_or_else(|| {
                eprintln!("block-mid-journal-write needs a -- before the final edit");
                std::process::exit(2);
            });
            let (setup_edits, final_edit) = rest.split_at(separator);
            let final_edit = final_edit
                .get(1)
                .unwrap_or_else(|| {
                    eprintln!("block-mid-journal-write needs exactly one edit after --");
                    std::process::exit(2);
                })
                .clone();

            for edit in setup_edits {
                host.apply_edit(edit.clone()).expect("apply_edit");
            }

            // The record this edit *would* have become, if the write had
            // completed. Only the first `n_bytes` of it actually reach the
            // file.
            let next_sequence = host.last_presented_sequence() + 1;
            let encoded_len = recovery_harness::journal::JournalRecord {
                sequence: next_sequence,
                payload: final_edit.clone().into_bytes(),
            }
            .encode()
            .len();
            let n_bytes = n_bytes.min(encoded_len);

            host.journal_writer_mut().set_next_fault(
                recovery_harness::journal::WriteFault::FailAfterPartialWrite(n_bytes),
            );
            // Deliberately ignored: `FailAfterPartialWrite` always returns
            // `Err` after writing exactly `n_bytes`, which is the point --
            // this is not a bug in the child, it is the fault this mode
            // exists to produce.
            let _ = host.journal_writer_mut().append(
                next_sequence,
                final_edit.as_bytes(),
                matches!(
                    policy.journal,
                    recovery_harness::JournalMode::AppendEveryEdit { fsync: true }
                ),
            );

            print_ready();
            block_forever();
        }
        "block-mid-checkpoint-write" => {
            let Some((n_bytes_arg, edits)) = rest.split_first() else {
                usage();
            };
            let n_bytes: usize = n_bytes_arg.parse().expect("n_bytes is a number");

            for edit in edits {
                host.apply_edit(edit.clone()).expect("apply_edit");
            }

            let checkpoint = recovery_harness::checkpoint::Checkpoint {
                sequence: host.last_presented_sequence(),
                content: host.document().as_bytes().to_vec(),
            };
            let fault = recovery_harness::checkpoint::WriteFault::FailAfterPartialWrite(n_bytes);
            // Deliberately ignored, for the same reason as above: the
            // partial write and the resulting `Err` are the fault, not an
            // accident.
            let _ = recovery_harness::checkpoint::write_checkpoint(
                &host.checkpoint_path(),
                &checkpoint,
                fault,
            );

            print_ready();
            block_forever();
        }
        other => {
            eprintln!("unknown mode: {other}");
            usage();
        }
    }
}
