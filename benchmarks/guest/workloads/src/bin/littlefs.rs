#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

#[cfg(target_os = "none")]
use rv32im_guest::guest_entry;
use rv32im_workloads::run_c;

#[link(name = "rvb_c_workloads", kind = "static")]
unsafe extern "C" {
    fn rvb_littlefs(input: *const u8, input_len: u32, output: *mut u32) -> u32;
}

fn littlefs(input: &[u8]) -> [u8; 16] {
    run_c(input, 23, rvb_littlefs)
}

#[cfg(target_os = "none")]
fn guest_main(input: &[u8]) -> u32 {
    rv32im_workloads::emit(&rv32im_workloads::run(littlefs, input))
}

#[cfg(target_os = "none")]
guest_entry!(guest_main);

#[cfg(not(target_os = "none"))]
fn main() -> std::process::ExitCode {
    rv32im_workloads::native::main(|input| rv32im_workloads::run(littlefs, input))
}
