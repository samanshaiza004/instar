//! FNV-1a, 32-bit, self-contained rather than a new dependency.
//!
//! Not chosen for collision resistance against an adversary -- there is no
//! adversary here, only torn writes and truncation. It is chosen because it
//! is five lines, has no dependency, and is exactly as good as any other
//! checksum at answering the one question this harness needs answered: did
//! this record's bytes change, or did the file simply end before they were
//! all written?

const FNV_OFFSET_BASIS: u32 = 0x811c_9dc5;
const FNV_PRIME: u32 = 0x0100_0193;

pub fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_the_offset_basis() {
        assert_eq!(fnv1a(&[]), FNV_OFFSET_BASIS);
    }

    #[test]
    fn one_flipped_bit_changes_the_hash() {
        let a = fnv1a(b"hello world");
        let b = fnv1a(b"hello worle"); // last byte flipped
        assert_ne!(
            a, b,
            "a checksum that can't see a single-byte change is not one"
        );
    }

    #[test]
    fn is_deterministic() {
        assert_eq!(fnv1a(b"hello world"), fnv1a(b"hello world"));
    }
}
