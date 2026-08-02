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

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
)))]
use rv32vm_rust_common::memory::NativeMemoryView;

pub(crate) const SIDE_EXIT_FLAG: u32 = 1 << 31;

/// A decoded guest instruction or its precise fetch trap.
pub type BlockInstruction = Result<DecodedInstruction, GuestTrap>;
pub use emitter::{
    CompiledBlock, MAX_BOUNDED_REGION_BLOCKS, MAX_BOUNDED_REGION_INSTRUCTIONS,
    MAX_GROUPED_LOOP_CODE_BYTES, MAX_LOOP_BLOCKS, MAX_LOOP_GROUP_FACTOR, MAX_LOOP_INSTRUCTIONS,
    MAX_REGION_BLOCKS, MAX_REGION_INSTRUCTIONS, NativeEntryKind, RegionBlock, RegionLimits,
};

/// Result of one native entry invocation.
///
/// A side exit identifies the faulting guest PC and excludes that instruction
/// from `retired`, allowing an engine to interpret exactly that one operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeOutcome {
    raw: u64,
}

impl NativeOutcome {
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self { raw }
    }

    pub const fn next_pc(self) -> u32 {
        self.raw as u32
    }

    pub const fn retired(self) -> u32 {
        (self.raw >> 32) as u32 & !SIDE_EXIT_FLAG
    }

    pub const fn needs_interpreter(self) -> bool {
        (self.raw >> 32) as u32 & SIDE_EXIT_FLAG != 0
    }
}

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

    pub const fn minimum_instruction_count(&self) -> usize {
        unreachable!()
    }

    pub const fn loop_unroll_factor(&self) -> usize {
        unreachable!()
    }

    pub const fn kind(&self) -> NativeEntryKind {
        unreachable!()
    }

    pub fn optimization_counts(&self, _retired: usize) -> (usize, usize) {
        unreachable!()
    }

    pub fn execute(
        &self,
        _registers: &mut [u32; 32],
        _memory: NativeMemoryView<'_>,
    ) -> NativeOutcome {
        unreachable!()
    }

    pub fn execute_with_limit(
        &self,
        _registers: &mut [u32; 32],
        _memory: NativeMemoryView<'_>,
        _remaining: u64,
    ) -> Option<NativeOutcome> {
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

    pub const fn minimum_instruction_count(&self) -> usize {
        unreachable!()
    }

    pub const fn loop_unroll_factor(&self) -> usize {
        unreachable!()
    }

    pub const fn kind(&self) -> NativeEntryKind {
        unreachable!()
    }

    pub fn execute(
        &self,
        _registers: &mut [u32; 32],
        _memory: NativeMemoryView<'_>,
    ) -> NativeOutcome {
        unreachable!()
    }

    pub fn execute_with_limit(
        &self,
        _registers: &mut [u32; 32],
        _memory: NativeMemoryView<'_>,
        _remaining: u64,
    ) -> Option<NativeOutcome> {
        unreachable!()
    }
}
