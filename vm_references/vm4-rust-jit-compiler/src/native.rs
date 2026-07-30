//! Selects the x86-64 Linux backend or a portable no-JIT fallback.

#[cfg(any(
    test,
    all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    )
))]
#[path = "x86_64/emitter.rs"]
mod emitter;

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[allow(unsafe_code)]
#[path = "x86_64/mod.rs"]
mod implementation;

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
pub(crate) use implementation::NativeBlock;

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
)))]
mod unsupported {
    use crate::block::BasicBlock;

    /// Placeholder native tier used on hosts without the x86-64 Linux backend.
    pub(crate) struct NativeBlock;

    impl NativeBlock {
        pub(crate) fn compile(_block: &BasicBlock, _code_budget: usize) -> Option<Self> {
            None
        }

        pub(crate) const fn mapped_len(&self) -> usize {
            0
        }

        pub(crate) const fn instruction_count(&self) -> usize {
            unreachable!()
        }

        pub(crate) fn execute(&self, _registers: &mut [u32; 32]) -> u32 {
            unreachable!()
        }
    }
}

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
)))]
pub(crate) use unsupported::NativeBlock;
