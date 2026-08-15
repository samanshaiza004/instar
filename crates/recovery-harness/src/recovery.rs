//! Recovery: load the checkpoint, if any, then replay every journal record
//! newer than it, in order, stopping the instant the journal stops being
//! trustworthy.
//!
//! Deliberately a free function taking two paths, not a `Host` method. What
//! it proves is meant to be checkable by something that was never the
//! process that wrote these files -- a fresh process, in the subprocess
//! tests, standing in for whatever comes after a real crash.

use std::io;
use std::path::Path;

use crate::checkpoint;
use crate::journal::{self, TailFault};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredState {
    /// The document as recovery reconstructs it.
    pub content: String,
    /// The newest edit sequence recovery is willing to vouch for -- what a
    /// future doc should call the recovery sequence, distinct from whatever
    /// sequence was last shown on screen before the failure.
    pub last_recovered_sequence: u64,
    /// How many journal records were actually folded into `content`, after
    /// skipping anything already covered by the checkpoint. A mutant that
    /// replays or drops a record changes this count without necessarily
    /// changing `content` in a way that's obvious by inspection, which is
    /// why it is reported as its own fact rather than left implicit.
    pub applied_from_journal: usize,
    pub tail_fault: Option<TailFault>,
    pub tail_offset: Option<u64>,
}

/// Recovers document state from whatever `checkpoint_path` and
/// `journal_path` currently hold, with no side effects: nothing here writes
/// to either file. Calling it twice against the same on-disk state must
/// produce the same `RecoveredState`, which is the whole of what "corrupt
/// tail handled deterministically" means as an assertion.
pub fn recover(journal_path: &Path, checkpoint_path: &Path) -> io::Result<RecoveredState> {
    let checkpoint = checkpoint::read_checkpoint_file(checkpoint_path)?;
    let (mut content, mut sequence) = match &checkpoint {
        Some(c) => (String::from_utf8_lossy(&c.content).into_owned(), c.sequence),
        None => (String::new(), 0),
    };

    let journal_result = journal::read_journal_file(journal_path)?;
    let mut applied = 0usize;

    // Every record whose sequence the checkpoint (or an earlier record
    // already folded in) already accounts for is skipped rather than
    // reapplied -- this is the whole of what makes "duplicate edit did not
    // replay twice" a property of the *loop bounds*, not of luck. A journal
    // truncated *after* a checkpoint but not yet pruned would otherwise
    // replay everything the checkpoint already captured.
    for record in &journal_result.records {
        if record.sequence <= sequence {
            continue;
        }
        content.push_str(&String::from_utf8_lossy(&record.payload));
        sequence = record.sequence;
        applied += 1;
    }

    Ok(RecoveredState {
        content,
        last_recovered_sequence: sequence,
        applied_from_journal: applied,
        tail_fault: journal_result.tail_fault,
        tail_offset: journal_result.tail_offset,
    })
}
