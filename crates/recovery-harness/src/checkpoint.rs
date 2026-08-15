//! One full-document snapshot, written atomically-enough to be checked for
//! completeness on read rather than trusted on write.
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
//! # Not atomic-by-rename, on purpose
//!
//! A production implementation would write to a temp file and rename over
//! the old checkpoint, making torn writes structurally impossible. This
//! harness writes in place instead, because the whole point of the
//! "process killed during checkpoint write" scenario is to prove the
//! *reader* detects a torn checkpoint deterministically -- and a mechanism
//! that made torn checkpoints impossible would make that scenario
//! unreachable rather than proven safe. Whichever policy is eventually
//! chosen may still prefer write-then-rename in production; this harness
//! proves the reader is correct either way, which is the part that has to
//! be correct regardless of which write strategy protects it.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::checksum::fnv1a;

const MAGIC: u32 = 0x4348_4B50;
const FIXED_OVERHEAD: usize = 4 + 8 + 4 + 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub sequence: u64,
    pub content: Vec<u8>,
}

impl Checkpoint {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(FIXED_OVERHEAD + self.content.len());
        bytes.extend_from_slice(&MAGIC.to_le_bytes());
        bytes.extend_from_slice(&self.sequence.to_le_bytes());
        bytes.extend_from_slice(&(self.content.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&self.content);
        let checksum = fnv1a(&bytes);
        bytes.extend_from_slice(&checksum.to_le_bytes());
        bytes
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
/// from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFault {
    None,
    FailBeforeWrite,
    FailAfterPartialWrite(usize),
}

/// Writes a checkpoint, in place, truncating whatever checkpoint was there
/// before. `fault` simulates the write being interrupted; the checkpoint
/// file is left exactly as many bytes as reached it before the fault --
/// which is what `decode` above then has to refuse.
pub fn write_checkpoint(path: &Path, checkpoint: &Checkpoint, fault: WriteFault) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    let bytes = checkpoint.encode();

    match fault {
        WriteFault::None => {
            file.write_all(&bytes)?;
            file.sync_all()?;
            Ok(())
        }
        WriteFault::FailBeforeWrite => Err(io::Error::other("injected: fail before write")),
        WriteFault::FailAfterPartialWrite(n) => {
            let n = n.min(bytes.len());
            file.write_all(&bytes[..n])?;
            file.sync_all()?;
            Err(io::Error::other("injected: fail after partial write"))
        }
    }
}

/// Directly overwrites the checkpoint file with `bytes`, no encoding. For
/// tests that construct a specific corrupt or truncated file by hand rather
/// than by interrupting a real write.
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
/// standing in for what a process death mid-`write_all` would leave behind.
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
        let bytes = checkpoint.encode();
        assert_eq!(decode(&bytes), Some(checkpoint));
    }

    #[test]
    fn a_truncated_checkpoint_decodes_to_none_not_a_partial_document() {
        let checkpoint = Checkpoint {
            sequence: 42,
            content: b"hello world".to_vec(),
        };
        let mut bytes = checkpoint.encode();
        bytes.truncate(bytes.len() - 4); // drop the checksum
        assert_eq!(decode(&bytes), None);
    }

    #[test]
    fn a_corrupted_byte_in_the_content_decodes_to_none() {
        let checkpoint = Checkpoint {
            sequence: 42,
            content: b"hello world".to_vec(),
        };
        let mut bytes = checkpoint.encode();
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
        let mut bytes = checkpoint.encode();
        bytes.truncate(bytes.len() - 1);
        assert_eq!(decode(&bytes), decode(&bytes));
    }
}
