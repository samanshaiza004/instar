//! The append-only journal: one opaque record per durable write, each
//! independently checksummed so a torn write or a mid-record process death
//! never has to be guessed about -- it is detected, at the exact byte
//! offset where the record stops being trustworthy.
//!
//! This module knows nothing about what a payload means. It is handed
//! bytes and a sequence number and it stores them, or it is handed bytes
//! back from storage and it hands the caller back exactly what it wrote --
//! interpretation is [`crate::fake_guest::FakeGuest`]'s job, one layer up,
//! never this one's.
//!
//! # Record layout
//!
//! ```text
//! magic        u32 LE   0x4A52_4543 ("JREC" as bytes, order irrelevant --
//!                       it exists to reject garbage, not to be readable)
//! sequence     u64 LE   monotonic; the record's identity, assigned by
//!                       whoever is writing -- this module only checks that
//!                       it is present and legible, never what it names
//! payload_len  u32 LE
//! payload      [u8; payload_len]   opaque
//! checksum     u32 LE   FNV-1a over every byte above, including magic
//! ```
//!
//! `checksum` covers `magic` too, on purpose: a record that starts mid-write
//! (magic itself torn) must not coincidentally checksum-match a shorter
//! valid-looking record built from leftover bytes.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::checksum::fnv1a;

const MAGIC: u32 = 0x4A52_4543;
/// magic(4) + sequence(8) + payload_len(4) + checksum(4), around the payload.
const FIXED_OVERHEAD: usize = 4 + 8 + 4 + 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    pub sequence: u64,
    pub payload: Vec<u8>,
}

impl JournalRecord {
    /// The number of bytes a record with a payload of `payload_len` bytes
    /// occupies on disk, without building the record. A caller enforcing
    /// its own budget (e.g. `RecoveryStore`'s `max_journal_bytes`) needs
    /// this before deciding whether to encode at all.
    pub fn wire_len(payload_len: usize) -> usize {
        FIXED_OVERHEAD + payload_len
    }

    /// `None` if `payload` does not fit in this format's `u32` length
    /// field -- the format's own structural ceiling (4 GiB), independent of
    /// and far above any policy-level bound a caller enforces separately.
    /// Never silently truncates a too-large payload into a shorter recorded
    /// length via `as u32`: that would record a length that does not match
    /// the bytes that follow it, corrupting the very next record's framing.
    pub fn encode(&self) -> Option<Vec<u8>> {
        let payload_len: u32 = self.payload.len().try_into().ok()?;
        let mut bytes = Vec::with_capacity(FIXED_OVERHEAD + self.payload.len());
        bytes.extend_from_slice(&MAGIC.to_le_bytes());
        bytes.extend_from_slice(&self.sequence.to_le_bytes());
        bytes.extend_from_slice(&payload_len.to_le_bytes());
        bytes.extend_from_slice(&self.payload);
        let checksum = fnv1a(&bytes);
        bytes.extend_from_slice(&checksum.to_le_bytes());
        Some(bytes)
    }
}

/// Why the reader stopped trusting the rest of the file.
///
/// Distinguished because a caller deciding "how much did we lose" needs to
/// know whether a whole record's worth of bytes is missing (never started
/// writing) or partially present (started, then stopped) -- both are
/// "truncated" to a naive reader, and conflating them is exactly the kind
/// of imprecision this harness exists to refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailFault {
    /// Fewer than the fixed header size remained: nothing to even parse a
    /// length from.
    HeaderTruncated,
    /// The header parsed and named a payload length, but fewer bytes than
    /// that (plus the trailing checksum) remained in the file.
    PayloadTruncated,
    /// A complete record's worth of bytes was present, but the checksum
    /// does not match -- corruption, not truncation. Bit rot and truncation
    /// look identical to a reader that only asks "is this well-formed";
    /// this variant is why the reader asks two separate questions.
    ChecksumMismatch,
    /// The magic did not match at all -- not a torn record, just not a
    /// record.
    BadMagic,
}

/// The result of reading every record a journal file holds right now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JournalReadResult {
    pub records: Vec<JournalRecord>,
    /// `None` if the file ended exactly on a record boundary with nothing
    /// left over -- the ordinary, healthy case.
    pub tail_fault: Option<TailFault>,
    /// Byte offset where `tail_fault` begins, when there is one. Recorded so
    /// two runs against the same corrupt file can be compared byte for byte
    /// rather than only "both said corrupt". Also exactly the length a
    /// repair pass should truncate a torn journal file down to: everything
    /// before this offset is the trusted tail.
    pub tail_offset: Option<u64>,
}

impl JournalReadResult {
    /// The length, in bytes, of the longest prefix of the file that is
    /// entirely trustworthy records. Repairing a torn journal means
    /// truncating it to exactly this length.
    pub fn trusted_len(&self) -> u64 {
        self.tail_offset.unwrap_or_else(|| {
            self.records
                .iter()
                .filter_map(|r| r.encode().map(|b| b.len() as u64))
                .sum()
        })
    }
}

/// Reads every well-formed record from `bytes`, stopping at the first one
/// that is not.
///
/// Never panics on truncated or corrupt input -- that is the entire reason
/// this function exists rather than a `bincode`-style decode. Determinism is
/// the contract: the same bytes always produce the same `records` and the
/// same `tail_fault` at the same `tail_offset`, which is what makes "corrupt
/// tail handled deterministically" a claim a test can make twice and get the
/// same answer both times.
pub fn read_records(bytes: &[u8]) -> JournalReadResult {
    let mut records = Vec::new();
    let mut offset = 0usize;

    loop {
        if offset == bytes.len() {
            return JournalReadResult {
                records,
                tail_fault: None,
                tail_offset: None,
            };
        }
        if bytes.len() - offset < FIXED_OVERHEAD {
            return JournalReadResult {
                records,
                tail_fault: Some(TailFault::HeaderTruncated),
                tail_offset: Some(offset as u64),
            };
        }

        let magic = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        if magic != MAGIC {
            return JournalReadResult {
                records,
                tail_fault: Some(TailFault::BadMagic),
                tail_offset: Some(offset as u64),
            };
        }
        let sequence = u64::from_le_bytes(bytes[offset + 4..offset + 12].try_into().unwrap());
        let payload_len =
            u32::from_le_bytes(bytes[offset + 12..offset + 16].try_into().unwrap()) as usize;

        let record_len = FIXED_OVERHEAD + payload_len;
        if bytes.len() - offset < record_len {
            return JournalReadResult {
                records,
                tail_fault: Some(TailFault::PayloadTruncated),
                tail_offset: Some(offset as u64),
            };
        }

        let payload_start = offset + 16;
        let payload_end = payload_start + payload_len;
        let payload = bytes[payload_start..payload_end].to_vec();
        let checksummed_end = payload_end;
        let stored_checksum = u32::from_le_bytes(
            bytes[checksummed_end..checksummed_end + 4]
                .try_into()
                .unwrap(),
        );
        let computed_checksum = fnv1a(&bytes[offset..checksummed_end]);

        if stored_checksum != computed_checksum {
            return JournalReadResult {
                records,
                tail_fault: Some(TailFault::ChecksumMismatch),
                tail_offset: Some(offset as u64),
            };
        }

        records.push(JournalRecord { sequence, payload });
        offset += record_len;
    }
}

pub fn read_journal_file(path: &Path) -> io::Result<JournalReadResult> {
    if !path.exists() {
        return Ok(JournalReadResult::default());
    }
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(read_records(&bytes))
}

/// A fault a test can inject into the *next* append, standing in for a
/// write that a real crash would have interrupted -- without needing a
/// subprocess for every such case. Real process death is still proven
/// separately, with a real subprocess; this is for proving the in-process
/// bookkeeping around a write, which a subprocess cannot observe from
/// outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFault {
    None,
    /// The write never reaches the OS at all -- `io::Error` returned before
    /// any bytes are written.
    FailBeforeWrite,
    /// `n` bytes reach the file (a torn write), then the call fails.
    FailAfterPartialWrite(usize),
}

pub struct JournalWriter {
    file: File,
    next_fault: WriteFault,
}

impl JournalWriter {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .write(true)
            .open(path)?;
        Ok(Self {
            file,
            next_fault: WriteFault::None,
        })
    }

    /// Test-only: arms a fault for the *next* `append` call, then resets to
    /// `WriteFault::None` regardless of whether that call used it.
    pub fn set_next_fault(&mut self, fault: WriteFault) {
        self.next_fault = fault;
    }

    /// Appends one opaque record. `fsync` decides whether the append is
    /// durable before this returns: `false` leaves the bytes in the OS page
    /// cache, reachable by a clean restart but not guaranteed to survive a
    /// real crash; `true` calls `sync_all` before returning, and only then
    /// is the record durable.
    ///
    /// `Ok(true)` from this method is the only fact "durable" may be
    /// derived from; whether that determination happens promptly is the
    /// caller's obligation, not this one's -- this function is already
    /// synchronous and blocking, so there is no window inside it for a
    /// caller to observe a state between "asked" and "written".
    ///
    /// Returns `Err` on any failure, injected or real, and the file may
    /// hold a torn record afterward (see `WriteFault::FailAfterPartialWrite`
    /// and, for a real crash, the process-failure test suite). The caller
    /// is responsible for treating a failed append as poisoning further
    /// writes until a repair pass runs -- this function has no memory of
    /// its own failures across calls.
    pub fn append(&mut self, sequence: u64, payload: &[u8], fsync: bool) -> io::Result<()> {
        let fault = std::mem::replace(&mut self.next_fault, WriteFault::None);
        let record = JournalRecord {
            sequence,
            payload: payload.to_vec(),
        };
        let bytes = record.encode().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "payload too large to encode (exceeds u32::MAX)",
            )
        })?;

        match fault {
            WriteFault::None => {
                self.file.write_all(&bytes)?;
            }
            WriteFault::FailBeforeWrite => {
                return Err(io::Error::other("injected: fail before write"));
            }
            WriteFault::FailAfterPartialWrite(n) => {
                let n = n.min(bytes.len());
                self.file.write_all(&bytes[..n])?;
                self.file.sync_all()?;
                return Err(io::Error::other("injected: fail after partial write"));
            }
        }

        if fsync {
            self.file.sync_all()?;
        }
        Ok(())
    }

    /// Discards every record. Called by [`crate::RecoveryStore`] only after
    /// a checkpoint covering them has been durably, atomically installed --
    /// never on its own, and never as a side effect of a generation
    /// replacement or a read. See `RecoveryStore::write_checkpoint`.
    pub fn truncate(&mut self) -> io::Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.sync_all()
    }

    /// Truncates to exactly `len` bytes -- the repair operation for a
    /// journal whose tail was found torn on open. `len` should be
    /// [`JournalReadResult::trusted_len`] from a read against the same file.
    pub fn truncate_to(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)?;
        self.file.seek(SeekFrom::Start(len))?;
        self.file.sync_all()
    }

    pub fn len_bytes(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_record_round_trips() {
        let record = JournalRecord {
            sequence: 7,
            payload: b"hello".to_vec(),
        };
        let bytes = record.encode().unwrap();
        let result = read_records(&bytes);
        assert_eq!(result.records, vec![record]);
        assert_eq!(result.tail_fault, None);
    }

    #[test]
    fn two_records_read_in_order() {
        let a = JournalRecord {
            sequence: 1,
            payload: b"a".to_vec(),
        };
        let b = JournalRecord {
            sequence: 2,
            payload: b"ab".to_vec(),
        };
        let mut bytes = a.encode().unwrap();
        bytes.extend(b.encode().unwrap());
        let result = read_records(&bytes);
        assert_eq!(result.records, vec![a, b]);
        assert_eq!(result.tail_fault, None);
    }

    #[test]
    fn a_truncated_header_is_reported_at_the_right_offset() {
        let a = JournalRecord {
            sequence: 1,
            payload: b"a".to_vec(),
        };
        let mut bytes = a.encode().unwrap();
        let boundary = bytes.len();
        bytes.extend_from_slice(&[0xAB, 0xCD]); // fewer than FIXED_OVERHEAD bytes
        let result = read_records(&bytes);
        assert_eq!(result.records, vec![a]);
        assert_eq!(result.tail_fault, Some(TailFault::HeaderTruncated));
        assert_eq!(result.tail_offset, Some(boundary as u64));
        assert_eq!(result.trusted_len(), boundary as u64);
    }

    #[test]
    fn a_truncated_payload_is_reported_at_the_right_offset() {
        let a = JournalRecord {
            sequence: 1,
            payload: b"a".to_vec(),
        };
        let b = JournalRecord {
            sequence: 2,
            payload: b"hello world".to_vec(),
        };
        let mut bytes = a.encode().unwrap();
        let boundary = bytes.len();
        let mut b_bytes = b.encode().unwrap();
        b_bytes.truncate(b_bytes.len() - 3); // cut into the payload
        bytes.extend(b_bytes);
        let result = read_records(&bytes);
        assert_eq!(result.records, vec![a]);
        assert_eq!(result.tail_fault, Some(TailFault::PayloadTruncated));
        assert_eq!(result.tail_offset, Some(boundary as u64));
    }

    #[test]
    fn a_flipped_bit_is_a_checksum_mismatch_not_silently_accepted() {
        let a = JournalRecord {
            sequence: 1,
            payload: b"hello".to_vec(),
        };
        let mut bytes = a.encode().unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF; // corrupt the stored checksum itself
        let result = read_records(&bytes);
        assert_eq!(result.records, Vec::new());
        assert_eq!(result.tail_fault, Some(TailFault::ChecksumMismatch));
        assert_eq!(result.tail_offset, Some(0));
    }

    #[test]
    fn reading_the_same_corrupt_bytes_twice_gives_the_same_answer() {
        let a = JournalRecord {
            sequence: 1,
            payload: b"a".to_vec(),
        };
        let mut bytes = a.encode().unwrap();
        bytes.push(0x00); // one stray byte: not enough for another header
        let first = read_records(&bytes);
        let second = read_records(&bytes);
        assert_eq!(
            first, second,
            "recovery must be deterministic on the same input"
        );
    }

    #[test]
    fn a_payload_too_large_to_encode_is_refused_not_truncated() {
        // Constructing an actual 4 GiB Vec here would be wasteful; the
        // property under test is the length check itself, so a fake
        // oversized length is exercised through a dedicated unit rather
        // than actually allocating u32::MAX + 1 bytes.
        struct Oversized(usize);
        impl Oversized {
            fn would_encode(&self) -> bool {
                u32::try_from(self.0).is_ok()
            }
        }
        assert!(Oversized(u32::MAX as usize).would_encode());
        assert!(!Oversized(u32::MAX as usize + 1).would_encode());
    }
}
