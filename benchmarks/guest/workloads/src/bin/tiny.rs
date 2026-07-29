#![no_std]
#![no_main]

//! Minimal guest work for measuring fixed `RUN` and interpreter overhead.

use rv32im_guest::guest_entry;
use rv32im_workloads::{emit, Words};

fn main(input: &[u8]) -> u32 {
    let words = Words::new(input);
    let a = words.get(0);
    let b = words.get(1);
    let result = a.rotate_left(5) ^ b.rotate_right(3) ^ 0x7469_6e79;
    emit(1, result, input.len() as u32)
}

guest_entry!(main);
