#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

//! Minimal guest work for measuring fixed `RUN` and interpreter overhead.

#[cfg(target_os = "none")]
use rv32im_guest::guest_entry;
use rv32im_workloads::{encode_result, join_u32, Words};

fn tiny(input: &[u8]) -> [u8; 8] {
    let words = Words::new(input);
    let a = words.get(0);
    let b = words.get(1);
    let result = a.rotate_left(5) ^ b.rotate_right(3) ^ 0x7469_6e79;
    encode_result(join_u32(result, input.len() as u32))
}

#[cfg(target_os = "none")]
fn guest_main(input: &[u8]) -> u32 {
    rv32im_workloads::emit(&rv32im_workloads::run(tiny, input))
}

#[cfg(target_os = "none")]
guest_entry!(guest_main);

#[cfg(not(target_os = "none"))]
fn main() -> std::process::ExitCode {
    rv32im_workloads::native::main(|input| rv32im_workloads::run(tiny, input))
}
