//! Compiles supported RV32IM instruction blocks into x86-64 code.

mod emitter;
mod lowering;

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[allow(unsafe_code)]
mod memory;

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[allow(unsafe_code)]
mod native;

#[cfg(test)]
mod test_support;

use rv32vm_rust_common::{GuestTrap, machine::DecodedInstruction};

/// A decoded guest instruction or its precise fetch trap.
pub type BlockInstruction = Result<DecodedInstruction, GuestTrap>;
pub use emitter::CompiledBlock;

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
pub use native::{NativeBlock, NativeEntry, NativeProgram};

/// Returns whether the block compiler can execute this instruction natively.
pub fn supports(instruction: DecodedInstruction) -> bool {
    lowering::Lowering::decode(instruction).is_some()
}

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
)))]
pub struct NativeProgram;

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
)))]
impl NativeProgram {
    pub fn publish(_blocks: Vec<CompiledBlock>, _code_budget: usize) -> Option<Self> {
        None
    }

    pub fn entry(&self, _index: usize) -> Option<NativeEntry<'_>> {
        None
    }

    pub const fn mapped_len(&self) -> usize {
        0
    }
}

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
)))]
#[derive(Clone, Copy)]
pub struct NativeEntry<'a>(std::marker::PhantomData<&'a ()>);

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
)))]
impl NativeEntry<'_> {
    pub const fn instruction_count(&self) -> usize {
        unreachable!()
    }

    pub fn execute(&self, _registers: &mut [u32; 32]) -> u32 {
        unreachable!()
    }
}

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
)))]
pub struct NativeBlock;

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
)))]
impl NativeBlock {
    pub fn publish(_block: CompiledBlock, _code_budget: usize) -> Option<Self> {
        None
    }

    pub const fn mapped_len(&self) -> usize {
        0
    }

    pub const fn instruction_count(&self) -> usize {
        unreachable!()
    }

    pub fn execute(&self, _registers: &mut [u32; 32]) -> u32 {
        unreachable!()
    }
}
