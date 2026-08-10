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

/// Owns one read-execute mapping containing finalized native code.
pub(crate) struct ExecutableMemory {
    address: NonNull<u8>,
    length: usize,
}

impl ExecutableMemory {
    pub(crate) fn publish(code: &[u8], byte_budget: usize) -> Option<Self> {
        // SAFETY: `getpagesize` takes no arguments and has no side effects
        // relevant to Rust memory safety.
        let page_size = usize::try_from(unsafe { getpagesize() }).ok()?;
        let length = mapping_length(code.len(), page_size, byte_budget)?;

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

    pub(crate) const fn address(&self) -> *const u8 {
        self.address.as_ptr()
    }

    pub(crate) const fn len(&self) -> usize {
        self.length
    }
}

fn mapping_length(code_len: usize, page_size: usize, byte_budget: usize) -> Option<usize> {
    if code_len == 0 || page_size == 0 {
        return None;
    }
    let pages = code_len.checked_add(page_size - 1)? / page_size;
    let length = pages.checked_mul(page_size)?;
    (length <= byte_budget).then_some(length)
}

impl Drop for ExecutableMemory {
    fn drop(&mut self) {
        // SAFETY: This owner is the sole holder of the complete mapping.
        unsafe {
            munmap(self.address.as_ptr().cast(), self.length);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mapping_length;

    #[test]
    fn mapping_length_obeys_exact_page_boundaries_and_budgets() {
        assert_eq!(mapping_length(0, 4_096, usize::MAX), None);
        assert_eq!(mapping_length(1, 4_096, 4_095), None);
        assert_eq!(mapping_length(1, 4_096, 4_096), Some(4_096));
        assert_eq!(mapping_length(4_096, 4_096, 4_096), Some(4_096));
        assert_eq!(mapping_length(4_097, 4_096, 8_192), Some(8_192));
        assert_eq!(mapping_length(usize::MAX, 4_096, usize::MAX), None);
    }
}
