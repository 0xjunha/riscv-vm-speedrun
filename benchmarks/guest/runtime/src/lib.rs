#![no_std]

//! Minimal runtime for the benchmark RV32IM execution environment.

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

const SYSCALL_EXIT: u32 = 0;
const SYSCALL_WRITE_OUTPUT: u32 = 1;
const PANIC_EXIT_CODE: u32 = 101;

global_asm!(
    r#"
    .option push
    .option norvc
    .section .text._start,"ax",@progbits
    .globl _start
    .type _start,@function
    .p2align 2
_start:
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop
    andi sp, sp, -16
    call __guest_main
    li a7, 0
    ecall
.Lunexpected_return:
    j .Lunexpected_return
    .size _start, .-_start
    .option pop
"#
);

#[inline]
pub fn write_output(bytes: &[u8]) -> u32 {
    let mut result = bytes.as_ptr() as u32;
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") result,
            in("a1") bytes.len() as u32,
            in("a7") SYSCALL_WRITE_OUTPUT,
            options(nostack)
        );
    }
    result
}

#[inline]
fn exit(code: u32) -> ! {
    unsafe {
        asm!(
            "ecall",
            in("a0") code,
            in("a7") SYSCALL_EXIT,
            options(noreturn, nostack)
        );
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    exit(PANIC_EXIT_CODE)
}

#[macro_export]
macro_rules! guest_entry {
    ($main:path) => {
        #[no_mangle]
        pub extern "C" fn __guest_main(input: *const u8, input_len: usize) -> u32 {
            let input = unsafe { core::slice::from_raw_parts(input, input_len) };
            $main(input)
        }
    };
}
