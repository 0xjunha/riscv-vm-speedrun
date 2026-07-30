//! Publishes generated code with a one-way writable-to-executable transition.

use std::{
    ffi::{c_int, c_void},
    ptr::{self, NonNull},
};

const PROT_READ: c_int = 0x1;
const PROT_WRITE: c_int = 0x2;
const PROT_EXEC: c_int = 0x4;
const MAP_PRIVATE: c_int = 0x2;
const MAP_ANONYMOUS: c_int = 0x20;

unsafe extern "C" {
    fn getpagesize() -> c_int;
    fn mmap(
        address: *mut c_void,
        length: usize,
        protection: c_int,
        flags: c_int,
        file: c_int,
        offset: i64,
    ) -> *mut c_void;
    fn mprotect(address: *mut c_void, length: usize, protection: c_int) -> c_int;
    fn munmap(address: *mut c_void, length: usize) -> c_int;
}

/// Owns one read-execute mapping containing a finalized native block.
pub(super) struct ExecutableMemory {
    address: NonNull<u8>,
    length: usize,
}

impl ExecutableMemory {
    pub(super) fn publish(code: &[u8], byte_budget: usize) -> Option<Self> {
        if code.is_empty() {
            return None;
        }
        // SAFETY: `getpagesize` takes no arguments and has no side effects
        // relevant to Rust memory safety.
        let page_size = usize::try_from(unsafe { getpagesize() }).ok()?;
        if page_size == 0 {
            return None;
        }
        let length = code.len().checked_add(page_size - 1)? / page_size * page_size;
        if length > byte_budget {
            return None;
        }

        // SAFETY: This requests a new private anonymous mapping and checks the
        // returned sentinel before constructing an owner.
        let address = unsafe {
            mmap(
                ptr::null_mut(),
                length,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if address as isize == -1 {
            return None;
        }
        let Some(address) = NonNull::new(address.cast::<u8>()) else {
            // SAFETY: A successful mapping at address zero is still owned and
            // must be released before representing it as a Rust pointer.
            unsafe {
                munmap(address, length);
            }
            return None;
        };

        // SAFETY: The new mapping is uniquely owned, writable, and at least
        // `length` bytes; `code.len()` is no larger than that allocation.
        unsafe {
            ptr::copy_nonoverlapping(code.as_ptr(), address.as_ptr(), code.len());
        }
        // SAFETY: The call covers exactly the live mapping. Failure leaves the
        // mapping owned here so it can be unmapped before returning.
        if unsafe { mprotect(address.as_ptr().cast(), length, PROT_READ | PROT_EXEC) } != 0 {
            // SAFETY: `address` and `length` still identify the owned mapping.
            unsafe {
                munmap(address.as_ptr().cast(), length);
            }
            return None;
        }
        Some(Self { address, length })
    }

    pub(super) const fn address(&self) -> *const u8 {
        self.address.as_ptr()
    }

    pub(super) const fn len(&self) -> usize {
        self.length
    }
}

impl Drop for ExecutableMemory {
    fn drop(&mut self) {
        // SAFETY: This owner is the sole holder of the complete mapping.
        unsafe {
            munmap(self.address.as_ptr().cast(), self.length);
        }
    }
}
