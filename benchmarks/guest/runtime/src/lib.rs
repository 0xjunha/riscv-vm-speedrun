#![no_std]

//! Minimal runtime for the benchmark RV32IM execution environment.

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

const SYSCALL_EXIT: u32 = 0;
const SYSCALL_WRITE_OUTPUT: u32 = 1;
const PANIC_EXIT_CODE: u32 = 101;

// Define the guest startup code that prepares Rust execution and exits through the VM.
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

/// Append bytes to the VM output stream and return the number written.
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

/// Terminate the guest run with the given exit code.
#[inline]
fn exit(code: u32) -> ! {
    unsafe {
        asm!(
            "ecall",
            in("a0") code,
            in("a7") SYSCALL_EXIT,
            options(nostack)
        );
    }
    // A fallback loop to prevent guest execution from resuming if the exit ECALL returns.
    loop {
        unsafe { asm!("", options(nomem, nostack)) }
    }
}

/// Terminate the guest with a fixed failure code when Rust code panics.
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    exit(PANIC_EXIT_CODE)
}

/// Define the guest entry point that passes VM input to a workload function.
#[macro_export]
macro_rules! guest_entry {
    ($main:path) => {
        /// Run the selected workload with input supplied by the VM.
        ///
        /// # Safety
        ///
        /// `input` must reference `input_len` readable bytes for this call.
        #[no_mangle]
        pub unsafe extern "C" fn __guest_main(input: *const u8, input_len: usize) -> u32 {
            let input = unsafe { core::slice::from_raw_parts(input, input_len) };
            $main(input)
        }
    };
}
