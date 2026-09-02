//! CRC32C (Castagnoli), table-driven.
//!
//! Written here rather than pulled from a crate for two reasons: the project is
//! meant to be from scratch below the SQL layer, and it keeps the library at
//! zero dependencies.
//!
//! Castagnoli rather than the IEEE polynomial because it has better error
//! detection on short inputs and is what modern storage engines use.

/// The Castagnoli polynomial, bit-reflected.
const POLY: u32 = 0x82F6_3B78;

/// Byte-at-a-time lookup table, built at compile time.
const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Computes the CRC32C of `data`.
///
/// ```
/// assert_eq!(lastro::util::crc32c(b"123456789"), 0xE306_9283);
/// ```
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_resume(0xFFFF_FFFF, data) ^ 0xFFFF_FFFF
}

/// Continues a CRC over another chunk. `state` starts at `0xFFFF_FFFF` and the
/// final value must be inverted; [`crc32c`] does both for the single-shot case.
pub fn crc32c_resume(state: u32, data: &[u8]) -> u32 {
    let mut crc = state;
    for &byte in data {
        let index = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ TABLE[index];
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // The standard check value for CRC32C.
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"a"), 0xC1D0_4330);
    }

    #[test]
    fn resuming_matches_single_shot() {
        let data: Vec<u8> = (0u8..=255).cycle().take(5000).collect();
        let whole = crc32c(&data);

        let (head, tail) = data.split_at(1234);
        let mut state = 0xFFFF_FFFF;
        state = crc32c_resume(state, head);
        state = crc32c_resume(state, tail);
        assert_eq!(state ^ 0xFFFF_FFFF, whole);
    }

    #[test]
    fn detects_single_bit_flips() {
        let mut data = vec![0x5Au8; 512];
        let original = crc32c(&data);
        let bits = data.len() * 8;
        for i in 0..bits {
            let (byte, bit) = (i / 8, i % 8);
            data[byte] ^= 1u8 << bit;
            assert_ne!(crc32c(&data), original, "flip at byte {byte} bit {bit}");
            data[byte] ^= 1u8 << bit;
        }
    }
}
