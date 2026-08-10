//! Checked W^X executable-memory publication and ownership.

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
pub(super) struct ExecutableMemory {
    address: std::ptr::NonNull<u8>,
    length: usize,
    #[cfg(test)]
    unmap_status: std::sync::Arc<std::sync::atomic::AtomicI32>,
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
impl ExecutableMemory {
    pub(super) fn publish(code: &[u8], byte_budget: usize) -> Option<Self> {
        use std::{ffi::c_void, ptr};

        const PROT_READ: i32 = 0x1;
        const PROT_WRITE: i32 = 0x2;
        const PROT_EXEC: i32 = 0x4;
        const MAP_PRIVATE: i32 = 0x2;
        const MAP_ANONYMOUS: i32 = 0x20;
        unsafe extern "C" {
            fn getpagesize() -> i32;
            fn mmap(
                address: *mut c_void,
                length: usize,
                protection: i32,
                flags: i32,
                file: i32,
                offset: i64,
            ) -> *mut c_void;
            fn mprotect(address: *mut c_void, length: usize, protection: i32) -> i32;
            fn munmap(address: *mut c_void, length: usize) -> i32;
        }

        // SAFETY: `getpagesize` has no arguments and returns process metadata.
        let page_size = usize::try_from(unsafe { getpagesize() }).ok()?;
        let length = mapping_length(code.len(), page_size, byte_budget)?;
        // SAFETY: A fresh private anonymous mapping is checked before use.
        let raw = unsafe {
            mmap(
                ptr::null_mut(),
                length,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if raw as isize == -1 {
            return None;
        }
        let Some(address) = std::ptr::NonNull::new(raw.cast::<u8>()) else {
            // SAFETY: `raw` is the complete successful mapping.
            unsafe { munmap(raw, length) };
            return None;
        };
        // SAFETY: The mapping is uniquely owned, writable, and large enough.
        unsafe { ptr::copy_nonoverlapping(code.as_ptr(), address.as_ptr(), code.len()) };
        // SAFETY: This covers exactly the owned mapping and removes write access.
        if unsafe { mprotect(raw, length, PROT_READ | PROT_EXEC) } != 0 {
            // SAFETY: The mapping is still owned here after failed publication.
            unsafe { munmap(raw, length) };
            return None;
        }
        Some(Self {
            address,
            length,
            #[cfg(test)]
            unmap_status: std::sync::Arc::new(std::sync::atomic::AtomicI32::new(i32::MIN)),
        })
    }

    pub(super) const fn address(&self) -> *const u8 {
        self.address.as_ptr()
    }

    pub(super) const fn len(&self) -> usize {
        self.length
    }

    #[cfg(test)]
    pub(super) fn unmap_status(&self) -> std::sync::Arc<std::sync::atomic::AtomicI32> {
        std::sync::Arc::clone(&self.unmap_status)
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
impl Drop for ExecutableMemory {
    fn drop(&mut self) {
        unsafe extern "C" {
            fn munmap(address: *mut std::ffi::c_void, length: usize) -> i32;
        }
        // SAFETY: This owner holds the complete live mapping exactly once.
        let _status = unsafe { munmap(self.address.as_ptr().cast(), self.length) };
        #[cfg(test)]
        self.unmap_status
            .store(_status, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
)))]
pub(super) struct ExecutableMemory;

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
)))]
impl ExecutableMemory {
    pub(super) fn publish(_code: &[u8], _byte_budget: usize) -> Option<Self> {
        None
    }

    pub(super) const fn len(&self) -> usize {
        0
    }
}

pub(super) fn mapping_length(
    code_len: usize,
    page_size: usize,
    byte_budget: usize,
) -> Option<usize> {
    if code_len == 0 || page_size == 0 {
        return None;
    }
    let pages = code_len.checked_add(page_size - 1)? / page_size;
    let length = pages.checked_mul(page_size)?;
    (length <= byte_budget).then_some(length)
}
