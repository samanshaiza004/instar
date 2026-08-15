//! Recovery: load the checkpoint, if any, then walk every journal record
//! newer than it, in order, stopping the instant the journal stops being
//! trustworthy or its sequence stops being contiguous.
//!
//! This module hands back raw, opaque records -- a checkpoint's bytes and a
//! list of journal records' bytes -- and nothing else. It does not know what
//! a payload means, does not concatenate anything, and does not produce a
//! document. That interpretation belongs one layer up, in
//! [`crate::fake_guest::FakeGuest`], which is the only place in this crate
//! allowed to decide that a payload is a string to be appended.
//!
//! Deliberately a free function taking two paths, not a method on any
//! stateful type, and deliberately side-effect-free: it never writes to
//! either file, and calling it twice against the same on-disk state must
//! produce the same [`RecoveredRecords`] both times. That determinism, and
//! that non-destructiveness, is what lets a test call this function to
//! *inspect* recovery state without that inspection changing what a
//! subsequent real recovery would see.

use std::io;
use std::path::Path;

use crate::checkpoint::{self, Checkpoint};
use crate::journal::{self, JournalRecord, TailFault};

/// The journal's sequence numbers were not contiguous from where the
/// checkpoint (or the start of the journal, if there is none) left off.
/// Distinguished from a corrupt or truncated tail: a gap is a *missing*
/// record, one that was apparently never written or never durably
/// committed, not one that started writing and stopped partway through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceGap {
    /// The sequence recovery needed next, contiguous from whatever came
    /// before.
    pub expected: u64,
    /// The sequence actually found in the record where `expected` should
    /// have been.
    pub found: u64,
}

/// Everything recovery could reconstruct from disk, as raw opaque bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredRecords {
    pub checkpoint: Option<Checkpoint>,
    /// Every journal record strictly newer than the checkpoint (or all of
    /// them, if there is no checkpoint), in order, with anything whose
    /// sequence duplicates or predates already-recovered state skipped --
    /// this is what makes "duplicate edit did not replay twice" a property
    /// of this function's loop bounds, not of a caller's care. Stops at the
    /// first sequence gap or the first untrusted journal byte, whichever
    /// comes first; a gap and a tail fault are mutually exclusive, since a
    /// gap only advances past well-formed records to begin with.
    pub journal_records: Vec<JournalRecord>,
    pub tail_fault: Option<TailFault>,
    pub tail_offset: Option<u64>,
    pub sequence_gap: Option<SequenceGap>,
}

impl RecoveredRecords {
    /// The newest sequence this recovery is willing to vouch for: the last
    /// accepted journal record's sequence, or the checkpoint's, or `0` if
    /// there is neither.
    pub fn last_recovered_sequence(&self) -> u64 {
        self.journal_records
            .last()
            .map(|r| r.sequence)
            .or_else(|| self.checkpoint.as_ref().map(|c| c.sequence))
            .unwrap_or(0)
    }
}

/// Reads whatever `checkpoint_path` and `journal_path` currently hold, with
/// no side effects on either file.
pub fn read(journal_path: &Path, checkpoint_path: &Path) -> io::Result<RecoveredRecords> {
    let checkpoint = checkpoint::read_checkpoint_file(checkpoint_path)?;
    let base_sequence = checkpoint.as_ref().map(|c| c.sequence).unwrap_or(0);

    let journal_result = journal::read_journal_file(journal_path)?;

    let mut journal_records = Vec::new();
    let mut expected = base_sequence + 1;
    let mut sequence_gap = None;

    for record in journal_result.records {
        if record.sequence < expected {
            // Already covered by the checkpoint, or by an earlier record in
            // this same journal -- a stale or duplicate record, not an
            // error. A journal truncated after a checkpoint but not yet
            // pruned looks exactly like this.
            continue;
        }
        if record.sequence > expected {
            sequence_gap = Some(SequenceGap {
                expected,
                found: record.sequence,
            });
            break;
        }
        expected = record.sequence + 1;
        journal_records.push(record);
    }

    Ok(RecoveredRecords {
        checkpoint,
        journal_records,
        tail_fault: journal_result.tail_fault,
        tail_offset: journal_result.tail_offset,
        sequence_gap,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::JournalWriter;

    struct TestDir(std::path::PathBuf);
    impl TestDir {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "recovery-harness-recovery-test-{}-{}",
                std::process::id(),
                label
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn no_checkpoint_no_journal_recovers_nothing() {
        let dir = TestDir::new("empty");
        let journal_path = dir.path().join("journal.bin");
        let checkpoint_path = dir.path().join("checkpoint.bin");
        let result = read(&journal_path, &checkpoint_path).unwrap();
        assert_eq!(result.checkpoint, None);
        assert_eq!(result.journal_records, Vec::new());
        assert_eq!(result.last_recovered_sequence(), 0);
    }

    #[test]
    fn journal_records_covered_by_a_checkpoint_are_not_replayed() {
        let dir = TestDir::new("dedup");
        let journal_path = dir.path().join("journal.bin");
        let checkpoint_path = dir.path().join("checkpoint.bin");

        let mut writer = JournalWriter::open(&journal_path).unwrap();
        writer.append(1, b"a", true).unwrap();
        writer.append(2, b"b", true).unwrap();
        writer.append(3, b"c", true).unwrap();

        checkpoint::write_checkpoint_atomic(
            dir.path(),
            &Checkpoint {
                sequence: 2,
                content: b"ab".to_vec(),
            },
            checkpoint::WriteFault::None,
        )
        .unwrap();

        let result = read(&journal_path, &checkpoint_path).unwrap();
        assert_eq!(result.journal_records.len(), 1);
        assert_eq!(result.journal_records[0].sequence, 3);
        assert_eq!(result.last_recovered_sequence(), 3);
    }

    #[test]
    fn a_missing_sequence_is_reported_as_a_gap_and_stops_the_tail() {
        let dir = TestDir::new("gap");
        let journal_path = dir.path().join("journal.bin");
        let checkpoint_path = dir.path().join("checkpoint.bin");

        let mut writer = JournalWriter::open(&journal_path).unwrap();
        writer.append(1, b"a", true).unwrap();
        writer.append(3, b"c", true).unwrap(); // 2 is missing

        let result = read(&journal_path, &checkpoint_path).unwrap();
        assert_eq!(result.journal_records.len(), 1);
        assert_eq!(result.journal_records[0].sequence, 1);
        assert_eq!(
            result.sequence_gap,
            Some(SequenceGap {
                expected: 2,
                found: 3
            })
        );
    }

    #[test]
    fn reading_the_same_state_twice_gives_the_same_answer() {
        let dir = TestDir::new("determinism");
        let journal_path = dir.path().join("journal.bin");
        let checkpoint_path = dir.path().join("checkpoint.bin");
        let mut writer = JournalWriter::open(&journal_path).unwrap();
        writer.append(1, b"a", true).unwrap();
        drop(writer);

        let first = read(&journal_path, &checkpoint_path).unwrap();
        let second = read(&journal_path, &checkpoint_path).unwrap();
        assert_eq!(first, second);
    }
}
