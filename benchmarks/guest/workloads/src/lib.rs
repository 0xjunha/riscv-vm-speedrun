#![cfg_attr(target_os = "none", no_std)]

//! Shared support for the public benchmark workloads.

#[cfg(not(target_os = "none"))]
pub mod native;

const OUTPUT_MAGIC: u32 = 0x3142_5652; // bytes: RVB1

/// Native or guest implementation of one public workload.
pub type Workload = fn(&[u8]) -> [u8; 16];

/// Run a workload, repeating when the `long` feature is enabled.
#[inline]
pub fn run(workload: Workload, input: &[u8]) -> [u8; 16] {
    #[cfg(not(feature = "long"))]
    {
        workload(input)
    }
    #[cfg(feature = "long")]
    {
        let Some((repetitions, input)) = input.split_at_checked(4) else {
            return workload(input);
        };
        let repetitions = u32::from_le_bytes(repetitions.try_into().unwrap());
        let workload = core::hint::black_box(workload);
        let mut output = [0; 16];
        for _ in 0..repetitions {
            output = core::hint::black_box(workload(core::hint::black_box(input)));
        }
        output
    }
}

#[cfg(all(test, feature = "long"))]
mod tests {
    use core::sync::atomic::{AtomicU32, Ordering};

    use super::run;

    static CALLS: AtomicU32 = AtomicU32::new(0);

    fn counted(input: &[u8]) -> [u8; 16] {
        CALLS.fetch_add(1, Ordering::Relaxed);
        [input.len() as u8; 16]
    }

    #[test]
    fn long_input_repeats_workload() {
        CALLS.store(0, Ordering::Relaxed);
        assert_eq!(run(counted, &[3, 0, 0, 0, 1, 2]), [2; 16]);
        assert_eq!(CALLS.load(Ordering::Relaxed), 3);
    }
}

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

/// Encode the common 16-byte result record.
pub fn encode_output(family: u32, result: u32, auxiliary: u32) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&OUTPUT_MAGIC.to_le_bytes());
    bytes[4..8].copy_from_slice(&family.to_le_bytes());
    bytes[8..12].copy_from_slice(&result.to_le_bytes());
    bytes[12..16].copy_from_slice(&auxiliary.to_le_bytes());
    bytes
}

/// Compute the IEEE CRC-32 of a byte slice.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// Emit one result through the RV32IM execution environment.
#[cfg(target_os = "none")]
pub fn emit(result: &[u8; 16]) -> u32 {
    if rv32im_guest::write_output(result) == result.len() as u32 {
        0
    } else {
        2
    }
}
