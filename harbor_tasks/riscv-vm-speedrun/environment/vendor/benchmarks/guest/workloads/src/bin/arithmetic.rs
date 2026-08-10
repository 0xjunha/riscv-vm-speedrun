#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

//! Compute-heavy workload exercising integer, bitwise, and loop execution.

#[cfg(target_os = "none")]
use rv32im_guest::guest_entry;
use rv32im_workloads::{bounded, encode_result, join_u32, Words};

fn arithmetic(input: &[u8]) -> [u8; 8] {
    let words = Words::new(input);
    let iterations = bounded(words.get(0), 24_000, 120_000);
    let mut x = words.get(1) ^ 0x243f_6a88;
    let mut y = words.get(2) ^ 0x85a3_08d3;
    let mut step = 0x9e37_79b9u32;

    for _ in 0..iterations {
        x = x.wrapping_add(step);
        x ^= x.rotate_left(7);
        y = y.wrapping_add(x ^ x.wrapping_shr(3));
        y = y.rotate_right(11) ^ x;
        x = x.rotate_left(5).wrapping_add(y);
        step = step.wrapping_add(0x6d2b_79f5);
    }

    encode_result(join_u32(x, y ^ step))
}

#[cfg(target_os = "none")]
fn guest_main(input: &[u8]) -> u32 {
    rv32im_workloads::emit(&rv32im_workloads::run(arithmetic, input))
}

#[cfg(target_os = "none")]
guest_entry!(guest_main);

#[cfg(not(target_os = "none"))]
fn main() -> std::process::ExitCode {
    rv32im_workloads::native::main(|input| rv32im_workloads::run(arithmetic, input))
}
