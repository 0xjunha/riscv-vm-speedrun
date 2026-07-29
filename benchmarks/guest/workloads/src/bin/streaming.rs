#![no_std]
#![no_main]

//! Memory-read-heavy workload scanning guest input words sequentially.

use rv32im_guest::guest_entry;
use rv32im_workloads::{bounded, emit, Words};

fn main(input: &[u8]) -> u32 {
    let words = Words::new(input);
    let passes = bounded(words.get(0), 8, 32);
    let available = words.len().saturating_sub(2);
    let count = bounded(words.get(1), available.min(256), available.min(1_024));
    let mut sum = 0u32;
    let mut xor = 0u32;
    let mut weighted = 0u32;

    for pass in 0..passes {
        let mut stride = (pass as u32).wrapping_add(1);
        for index in 0..count {
            let value = words.get(index + 2);
            sum = sum.wrapping_add(value);
            xor ^= value.rotate_left(((index + pass) & 31) as u32);
            weighted = weighted.wrapping_add(value ^ stride);
            stride = stride.wrapping_add(0x9e37_79b9);
        }
    }

    emit(5, sum ^ xor, weighted)
}

guest_entry!(main);
