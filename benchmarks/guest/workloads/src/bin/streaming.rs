#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

//! Memory-read-heavy workload scanning guest input words sequentially.

#[cfg(target_os = "none")]
use rv32im_guest::guest_entry;
use rv32im_workloads::{bounded, encode_output, Words};

fn streaming(input: &[u8]) -> [u8; 16] {
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

    encode_output(5, sum ^ xor, weighted)
}

#[cfg(target_os = "none")]
fn guest_main(input: &[u8]) -> u32 {
    rv32im_workloads::emit(&rv32im_workloads::run(streaming, input))
}

#[cfg(target_os = "none")]
guest_entry!(guest_main);

#[cfg(not(target_os = "none"))]
fn main() -> std::process::ExitCode {
    rv32im_workloads::native::main(|input| rv32im_workloads::run(streaming, input))
}
