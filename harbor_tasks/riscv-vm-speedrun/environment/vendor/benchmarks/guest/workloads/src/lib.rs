#![cfg_attr(target_os = "none", no_std)]

//! Shared support for the public benchmark workloads.

#[cfg(not(target_os = "none"))]
pub mod native;

/// Native or guest implementation of one public workload.
pub type Workload = fn(&[u8]) -> [u8; 8];

/// ABI shared by the freestanding and host-native C workload adapters.
pub type CWorkload = unsafe extern "C" fn(*const u8, u32, *mut u32) -> u32;

/// Call an upstream-C workload while preserving the common output contract.
pub fn run_c(input: &[u8], workload: CWorkload) -> [u8; 8] {
    let Ok(input_len) = u32::try_from(input.len()) else {
        return encode_result(u64::from(u32::MAX));
    };
    let mut output = [0u32; 2];
    // SAFETY: `input` and `output` remain valid for the duration of the call,
    // and every registered adapter follows the `CWorkload` ABI.
    let status = unsafe { workload(input.as_ptr(), input_len, output.as_mut_ptr()) };
    if status == 0 {
        encode_result(join_u32(output[0], output[1]))
    } else {
        encode_result(u64::from(status))
    }
}

/// Run a workload, repeating when the `long` feature is enabled.
#[inline]
pub fn run(workload: Workload, input: &[u8]) -> [u8; 8] {
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
        let mut output = [0; 8];
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

    fn counted(input: &[u8]) -> [u8; 8] {
        CALLS.fetch_add(1, Ordering::Relaxed);
        [input.len() as u8; 8]
    }

    #[test]
    fn long_input_repeats_workload() {
        CALLS.store(0, Ordering::Relaxed);
        assert_eq!(run(counted, &[3, 0, 0, 0, 1, 2]), [2; 8]);
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
    pub fn word_count(&self) -> usize {
        self.bytes.len() / 4
    }
}

/// Use `default` for zero and otherwise cap the value at `maximum`.
#[inline]
pub fn bounded(raw: u32, default: usize, maximum: usize) -> usize {
    let value = if raw == 0 { default } else { raw as usize };
    value.min(maximum)
}

/// Join two independently computed 32-bit observations into one result.
pub fn join_u32(low: u32, high: u32) -> u64 {
    u64::from(low) | (u64::from(high) << 32)
}

/// Encode the common little-endian 64-bit result.
pub const fn encode_result(result: u64) -> [u8; 8] {
    result.to_le_bytes()
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
pub fn emit(result: &[u8; 8]) -> u32 {
    if rv32im_guest::write_output(result) == result.len() as u32 {
        0
    } else {
        2
    }
}
