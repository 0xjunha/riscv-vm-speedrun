#![no_std]

//! Shared input decoding and output encoding for the benchmark workloads.

use rv32im_guest::write_output;

const OUTPUT_MAGIC: u32 = 0x3142_5652; // bytes: RVB1

/// Little-endian 32-bit words backed by the guest input bytes.
pub struct Words<'a> {
    bytes: &'a [u8],
}

impl<'a> Words<'a> {
    /// Wrap the input bytes without copying them.
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// Read one word, returning zero when it is outside the complete input words.
    #[inline]
    pub fn get(&self, index: usize) -> u32 {
        let start = match index.checked_mul(4) {
            Some(value) => value,
            None => return 0,
        };
        let Some(chunk) = self.bytes.get(start..start.saturating_add(4)) else {
            return 0;
        };
        u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
    }

    /// Return the number of complete words in the input.
    pub fn len(&self) -> usize {
        self.bytes.len() / 4
    }

    /// Return whether the input contains no complete words.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Use `default` for zero and otherwise cap the value at `maximum`.
#[inline]
pub fn bounded(raw: u32, default: usize, maximum: usize) -> usize {
    let value = if raw == 0 { default } else { raw as usize };
    value.min(maximum)
}

/// Emit the common 16-byte result record, returning zero on success.
pub fn emit(family: u32, result: u32, auxiliary: u32) -> u32 {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&OUTPUT_MAGIC.to_le_bytes());
    bytes[4..8].copy_from_slice(&family.to_le_bytes());
    bytes[8..12].copy_from_slice(&result.to_le_bytes());
    bytes[12..16].copy_from_slice(&auxiliary.to_le_bytes());
    if write_output(&bytes) == bytes.len() as u32 {
        0
    } else {
        2
    }
}
