//! Counters for the copying this crate itself performs.
//!
//! # What these can and cannot claim
//!
//! Only what Instar does. `crop` is a B-tree, and the bytes it moves between
//! leaves when a node splits are invisible from here — claiming a total
//! "bytes copied" would be claiming knowledge this crate does not have.
//!
//! What it *can* claim is the number that matters architecturally:
//!
//! > How many times did something ask for the whole document, contiguously?
//!
//! That is the mistake a rope does not prevent, and the answer on an editing
//! path should be zero. [`Copying::whole_buffer_materializations`] is that
//! answer.
//!
//! # Why this does not catch every contiguous copy
//!
//! It counts calls to [`crate::TextSlice::materialize`], which is the only way
//! *this crate's API* produces an owned `String` from storage. An
//! implementation inside `storage.rs` that collected `crop`'s chunks directly
//! would bypass it.
//!
//! That gap is deliberate rather than unnoticed: `textbench` pairs these
//! counters with a counting allocator, which sees any contiguous copy however
//! it was built. The counter says *who* asked; the allocator says *whether it
//! happened*. Neither alone is enough.
//!
//! # Why thread-local rather than a field
//!
//! A field on `TextStorage` would need interior mutability — `materialize`
//! takes `&self` through a borrowed slice — and a `Cell` would make every
//! `TextStorage` `!Sync` to serve a diagnostic. Package C moves these
//! resources around a host that already has a guest thread; paying for that in
//! the type system to count materializations would be the wrong trade.

use std::cell::Cell;

/// Contiguous copies made through this crate's API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Copying {
    /// Calls to [`crate::TextSlice::materialize`].
    pub materializations: u64,
    /// Bytes those calls produced.
    pub materialized_bytes: u64,
    /// Materializations that covered the entire buffer.
    ///
    /// The architectural counter. On any latency-sensitive path this is zero,
    /// and a benchmark that reports otherwise has found the defect this crate
    /// exists to avoid.
    pub whole_buffer_materializations: u64,
}

thread_local! {
    static COPYING: Cell<Copying> = const {
        Cell::new(Copying {
            materializations: 0,
            materialized_bytes: 0,
            whole_buffer_materializations: 0,
        })
    };
}

/// The counts so far on this thread.
pub fn snapshot() -> Copying {
    COPYING.with(|copying| copying.get())
}

/// Zeroes the counts, so a caller can measure one operation.
pub fn reset() {
    COPYING.with(|copying| copying.set(Copying::default()));
}

pub(crate) fn record_materialization(bytes: usize, storage_len: usize) {
    COPYING.with(|copying| {
        let mut counts = copying.get();
        counts.materializations += 1;
        counts.materialized_bytes += bytes as u64;
        // An empty buffer is excluded: every insertion into one slices `0..0`,
        // and calling that a whole-buffer materialization would make the
        // counter fire loudest on the cheapest possible edit.
        if storage_len > 0 && bytes == storage_len {
            counts.whole_buffer_materializations += 1;
        }
        copying.set(counts);
    });
}
