//! One full-document snapshot, replacing the previous one atomically:
//! written to a temp file, fsynced, renamed over the old checkpoint, and the
//! containing directory fsynced -- so a crash at any point during a
//! checkpoint write either leaves the *previous* checkpoint fully intact, or
//! leaves the *new* one fully intact, and never a torn blend of both.
//!
//! # Record layout
//!
//! ```text
//! magic       u32 LE   0x4348_4B50 ("CHKP")
//! sequence    u64 LE   every edit up to and including this one is captured
//! content_len u32 LE
//! content     [u8; content_len]
//! checksum    u32 LE   FNV-1a over everything above
//! ```
//!
//! # Why rename, not in-place truncate-and-write
//!
//! An earlier version of this module wrote checkpoints in place
//! (`truncate(true)` then `write_all`), reasoning that "the reader detects a
//! torn checkpoint deterministically" was the property that mattered and
//! that in-place writes proved it without the complexity of a rename. That
//! reasoning was incomplete: an in-place write can destroy a *previously
//! durable* checkpoint the instant it truncates, before a single byte of the
//! replacement has landed. A crash in that window does not just leave the
//! *new* checkpoint torn (the case the old reader-focused test covered) --
//! it can leave *no* checkpoint at all, having overwritten the one write
//! that was already safely on disk. That is a strictly worse outcome than
//! "recovery falls back to the journal", and the old mechanism could not
//! avoid it. Real crash-lifecycle safety needs a write that cannot begin
//! destroying old data until the new data is already fully durable, which
//! is exactly what write-tmp / fsync-tmp / rename / fsync-parent-dir
//! guarantees: `rename` is atomic with respect to a concurrent reader or a
//! crash, so `checkpoint.bin` is, at every instant, either the old complete
//! checkpoint or the new complete checkpoint, never a mixture.
//!
//! The in-place, synthetic-corruption helpers (`write_raw`, `truncate_to`)
//! stay, but only for building a specific corrupt or truncated byte pattern
//! by hand to test the *decoder* in isolation -- not for simulating what a
//! real interrupted write leaves behind. That is now
//! [`write_checkpoint_atomic`]'s job, together with a real subprocess kill
//! for the process-lifecycle-shaped scenarios.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::checksum::fnv1a;

const MAGIC: u32 = 0x4348_4B50;
const FIXED_OVERHEAD: usize = 4 + 8 + 4 + 4;
const TMP_FILE_NAME: &str = "checkpoint.bin.tmp";
const FILE_NAME: &str = "checkpoint.bin";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub sequence: u64,
    pub content: Vec<u8>,
}

impl Checkpoint {
    /// `None` if `content` does not fit in this format's `u32` length field
    /// -- see `journal::JournalRecord::encode` for why this is a checked
    /// conversion rather than an `as u32` cast.
    pub fn encode(&self) -> Option<Vec<u8>> {
        let content_len: u32 = self.content.len().try_into().ok()?;
        let mut bytes = Vec::with_capacity(FIXED_OVERHEAD + self.content.len());
        bytes.extend_from_slice(&MAGIC.to_le_bytes());
        bytes.extend_from_slice(&self.sequence.to_le_bytes());
        bytes.extend_from_slice(&content_len.to_le_bytes());
        bytes.extend_from_slice(&self.content);
        let checksum = fnv1a(&bytes);
        bytes.extend_from_slice(&checksum.to_le_bytes());
        Some(bytes)
    }
}

/// Reading a checkpoint answers one question: is there a complete, intact
/// checkpoint here, or not. Unlike the journal there is nothing partial to
/// salvage from a torn checkpoint -- half a document is not a smaller valid
/// document, so any fault collapses to `None` rather than a partial value.
pub fn decode(bytes: &[u8]) -> Option<Checkpoint> {
    if bytes.len() < FIXED_OVERHEAD {
        return None;
    }
    let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    if magic != MAGIC {
        return None;
    }
    let sequence = u64::from_le_bytes(bytes[4..12].try_into().unwrap());
    let content_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let record_len = FIXED_OVERHEAD + content_len;
    if bytes.len() != record_len {
        // Not "< record_len": a checkpoint file is written once and never
        // appended to, so trailing garbage past a complete record is just
        // as untrustworthy as a short read -- there is no second record to
        // preserve by tolerating extra bytes.
        return None;
    }
    let content = bytes[16..16 + content_len].to_vec();
    let checksummed_end = 16 + content_len;
    let stored_checksum = u32::from_le_bytes(
        bytes[checksummed_end..checksummed_end + 4]
            .try_into()
            .unwrap(),
    );
    let computed_checksum = fnv1a(&bytes[0..checksummed_end]);
    if stored_checksum != computed_checksum {
        return None;
    }
    Some(Checkpoint { sequence, content })
}

pub fn checkpoint_path_for(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

fn tmp_path_for(dir: &Path) -> PathBuf {
    dir.join(TMP_FILE_NAME)
}

pub fn read_checkpoint_file(path: &Path) -> io::Result<Option<Checkpoint>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(decode(&bytes))
}

/// Mirrors `journal::WriteFault`: an in-process stand-in for a write a real
/// crash would have interrupted, for the cases a subprocess cannot observe
/// from outside. Applies only to the write into the *temp* file -- by
/// construction, any fault here leaves `checkpoint.bin` completely
/// untouched, which is the property this rewrite exists to guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFault {
    None,
    FailBeforeWrite,
    FailAfterPartialWrite(usize),
}

/// Writes `checkpoint` as the new checkpoint for the scope directory `dir`,
/// atomically: content lands fully in `checkpoint.bin.tmp` and is fsynced
/// there, `checkpoint.bin.tmp` is renamed over `checkpoint.bin`, and `dir`
/// itself is fsynced so the rename is durable too, not just visible to
/// readers in the same process.
///
/// A fault injected via `fault` interrupts only the write into the tmp
/// file. Whether that succeeds or fails, `checkpoint.bin` (the previous
/// checkpoint, if any) is byte-for-byte unchanged until the moment of
/// `rename`, which the OS performs as a single atomic operation -- there is
/// no window in which a reader, or a crash, can observe a partial rename.
pub fn write_checkpoint_atomic(
    dir: &Path,
    checkpoint: &Checkpoint,
    fault: WriteFault,
) -> io::Result<()> {
    let bytes = checkpoint.encode().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "checkpoint content too large to encode (exceeds u32::MAX)",
        )
    })?;
    let tmp_path = tmp_path_for(dir);
    let final_path = checkpoint_path_for(dir);

    let mut tmp = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_path)?;

    match fault {
        WriteFault::None => {
            tmp.write_all(&bytes)?;
            tmp.sync_all()?;
        }
        WriteFault::FailBeforeWrite => {
            return Err(io::Error::other("injected: fail before write"));
        }
        WriteFault::FailAfterPartialWrite(n) => {
            let n = n.min(bytes.len());
            tmp.write_all(&bytes[..n])?;
            tmp.sync_all()?;
            return Err(io::Error::other("injected: fail after partial write"));
        }
    }
    drop(tmp);

    std::fs::rename(&tmp_path, &final_path)?;

    let dir_handle = File::open(dir)?;
    dir_handle.sync_all()?;
    Ok(())
}

/// Removes a leftover `checkpoint.bin.tmp`, if one exists -- the residue of
/// a process that died between writing the tmp file and renaming it into
/// place. `checkpoint.bin` is unaffected either way (that is the entire
/// point of the rename design), so this is housekeeping, not recovery: safe
/// to call on every open, and safe to skip.
pub fn discard_stale_tmp(dir: &Path) -> io::Result<()> {
    match std::fs::remove_file(tmp_path_for(dir)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Directly overwrites the checkpoint file with `bytes`, no encoding. For
/// tests that construct a specific corrupt or truncated file by hand to
/// exercise `decode` in isolation, not for simulating an interrupted write
/// -- see the module docs for why those are no longer the same thing.
pub fn write_raw(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

pub fn delete(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Test-only seam for truncating a checkpoint file to exactly `len` bytes,
/// standing in for what a process death mid-`write_all` would leave behind
/// -- for decoder-only tests, per the module docs.
pub fn truncate_to(path: &Path, len: u64) -> io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(len)?;
    let mut file = file;
    file.seek(SeekFrom::Start(0))?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_checkpoint_round_trips() {
        let checkpoint = Checkpoint {
            sequence: 42,
            content: b"hello world".to_vec(),
        };
        let bytes = checkpoint.encode().unwrap();
        assert_eq!(decode(&bytes), Some(checkpoint));
    }

    #[test]
    fn a_truncated_checkpoint_decodes_to_none_not_a_partial_document() {
        let checkpoint = Checkpoint {
            sequence: 42,
            content: b"hello world".to_vec(),
        };
        let mut bytes = checkpoint.encode().unwrap();
        bytes.truncate(bytes.len() - 4); // drop the checksum
        assert_eq!(decode(&bytes), None);
    }

    #[test]
    fn a_corrupted_byte_in_the_content_decodes_to_none() {
        let checkpoint = Checkpoint {
            sequence: 42,
            content: b"hello world".to_vec(),
        };
        let mut bytes = checkpoint.encode().unwrap();
        let mid = 16 + 3; // inside the content region
        bytes[mid] ^= 0xFF;
        assert_eq!(decode(&bytes), None);
    }

    #[test]
    fn decoding_the_same_corrupt_bytes_twice_gives_the_same_answer() {
        let checkpoint = Checkpoint {
            sequence: 1,
            content: b"x".to_vec(),
        };
        let mut bytes = checkpoint.encode().unwrap();
        bytes.truncate(bytes.len() - 1);
        assert_eq!(decode(&bytes), decode(&bytes));
    }

    #[test]
    fn atomic_write_then_read_round_trips() {
        let tmp_dir = std::env::temp_dir().join(format!(
            "recovery-harness-checkpoint-test-{}-{}",
            std::process::id(),
            "round-trip"
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let checkpoint = Checkpoint {
            sequence: 5,
            content: b"atomic".to_vec(),
        };
        write_checkpoint_atomic(&tmp_dir, &checkpoint, WriteFault::None).unwrap();
        let read = read_checkpoint_file(&checkpoint_path_for(&tmp_dir)).unwrap();
        assert_eq!(read, Some(checkpoint));
        std::fs::remove_dir_all(&tmp_dir).ok();
    }

    #[test]
    fn a_fault_during_the_tmp_write_never_touches_an_existing_checkpoint() {
        let tmp_dir = std::env::temp_dir().join(format!(
            "recovery-harness-checkpoint-test-{}-{}",
            std::process::id(),
            "fault-preserves-old"
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();

        let old = Checkpoint {
            sequence: 1,
            content: b"old and durable".to_vec(),
        };
        write_checkpoint_atomic(&tmp_dir, &old, WriteFault::None).unwrap();

        let new = Checkpoint {
            sequence: 2,
            content: b"new checkpoint that will not finish writing".to_vec(),
        };
        let result = write_checkpoint_atomic(&tmp_dir, &new, WriteFault::FailAfterPartialWrite(4));
        assert!(result.is_err());

        // The old checkpoint must still be there, byte-for-byte -- this is
        // exactly the scenario the in-place mechanism could not guarantee.
        let read = read_checkpoint_file(&checkpoint_path_for(&tmp_dir)).unwrap();
        assert_eq!(read, Some(old));

        std::fs::remove_dir_all(&tmp_dir).ok();
    }
}
