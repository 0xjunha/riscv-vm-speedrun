//! Published linked-image ownership and native entry execution.

use rv32vm_rust_common::memory::DirectMemory;

#[cfg(feature = "profile")]
use super::register_cache::MAX_CACHED_REGISTERS;
use super::{
    DispatchTable, Emitter, EntryMetadata, ExecutableMemory, LinkedBlock, MAX_FIXED_CODE_BYTES,
    RegisterCache,
};
#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
use super::{EXIT_BUDGET, EXIT_INTERPRET_ONE, EXIT_MISSING, run_context::RunContext};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeStop {
    MissingSuccessor,
    Budget,
    InterpretOne,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NativeRun {
    pub(crate) pc: u32,
    pub(crate) retired: u64,
    pub(crate) stop: NativeStop,
    #[cfg(feature = "profile")]
    pub(crate) profile: NativeRunProfile,
}

#[cfg(feature = "profile")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct NativeRunProfile {
    pub(crate) blocks: u64,
    pub(crate) direct_links: u64,
    pub(crate) indirect_hits: u64,
    pub(crate) indirect_misses: u64,
    pub(crate) register_loads: u64,
    pub(crate) register_stores: u64,
    pub(crate) cache_fills: u64,
    pub(crate) cache_spills: u64,
    pub(crate) cache_read_hits: u64,
    pub(crate) cache_write_hits: u64,
    pub(crate) fallthrough_blocks: u64,
    pub(crate) branch_blocks: u64,
    pub(crate) jump_blocks: u64,
    pub(crate) memory_loads: u64,
    pub(crate) memory_stores: u64,
    pub(crate) direct_immediate: u64,
    pub(crate) direct_register: u64,
    pub(crate) direct_branch: u64,
    pub(crate) direct_memory_load: u64,
    pub(crate) direct_memory_store: u64,
}

/// Owns one fully relocated VM5 linked image.
pub(crate) struct LinkedProgram {
    pub(super) memory: ExecutableMemory,
    pub(super) entries: Vec<EntryMetadata>,
    pub(super) dispatch: DispatchTable,
    #[cfg(any(test, feature = "profile"))]
    pub(super) cache: RegisterCache,
    #[cfg(feature = "profile")]
    pub(super) hot_code_bytes: usize,
    #[cfg(feature = "profile")]
    pub(super) cold_code_bytes: usize,
    #[cfg(feature = "profile")]
    pub(super) external_thunk_bytes: usize,
    #[cfg(feature = "profile")]
    pub(super) shared_prologue_bytes: usize,
    #[cfg(feature = "profile")]
    pub(super) exit_trampoline_bytes: usize,
}

impl LinkedProgram {
    /// Mapping-independent fixed admission charge for the largest six-register
    /// shared entry/exit. Finalized code and profile sizes remain exact.
    pub(crate) const fn fixed_code_len() -> usize {
        MAX_FIXED_CODE_BYTES
    }

    #[cfg(test)]
    pub(crate) fn publish(blocks: Vec<LinkedBlock>, code_budget: usize) -> Option<Self> {
        Self::publish_with_code_len(blocks, code_budget).0
    }

    pub(crate) fn publish_with_code_len(
        blocks: Vec<LinkedBlock>,
        code_budget: usize,
    ) -> (Option<Self>, usize) {
        let reserved_len = if blocks.is_empty() {
            0
        } else {
            let Some(length) = blocks
                .iter()
                .try_fold(Self::fixed_code_len(), |total, block| {
                    total.checked_add(block.reserved_code_len())
                })
            else {
                return (None, 0);
            };
            length
        };
        let cache = RegisterCache::select(&blocks);
        let mut emitter = Emitter::new(cache);
        for block in &blocks {
            if emitter
                .emit_block(&block.instructions, block.flow, block.pc)
                .is_none()
            {
                return (None, 0);
            }
        }
        let Some(resolved) = emitter.resolve() else {
            return (None, 0);
        };
        let code_len = resolved.code.len();
        if code_len > reserved_len || code_len > code_budget {
            return (None, code_len);
        }
        let Some(dispatch) = DispatchTable::build(&resolved.code, &resolved.entries) else {
            return (None, code_len);
        };
        let entries = resolved
            .entries
            .into_iter()
            .map(|(_, metadata)| metadata)
            .collect();
        let program = ExecutableMemory::publish(&resolved.code, code_budget).map(|memory| Self {
            memory,
            entries,
            dispatch,
            #[cfg(any(test, feature = "profile"))]
            cache,
            #[cfg(feature = "profile")]
            hot_code_bytes: resolved.hot_code_bytes,
            #[cfg(feature = "profile")]
            cold_code_bytes: resolved.cold_code_bytes,
            #[cfg(feature = "profile")]
            external_thunk_bytes: resolved.external_thunk_bytes,
            #[cfg(feature = "profile")]
            shared_prologue_bytes: resolved.shared_prologue_bytes,
            #[cfg(feature = "profile")]
            exit_trampoline_bytes: resolved.exit_trampoline_bytes,
        });
        (program, code_len)
    }

    pub(crate) fn entry(&self, index: usize) -> Option<LinkedEntry<'_>> {
        Some(LinkedEntry {
            program: self,
            metadata: *self.entries.get(index)?,
        })
    }

    #[cfg(feature = "profile")]
    pub(crate) const fn mapped_len(&self) -> usize {
        self.memory.len()
    }

    #[cfg(feature = "profile")]
    pub(crate) const fn dispatch_pages(&self) -> usize {
        self.dispatch.page_count()
    }

    #[cfg(feature = "profile")]
    pub(crate) const fn dispatch_entries(&self) -> usize {
        self.dispatch.entry_count()
    }

    #[cfg(feature = "profile")]
    pub(crate) const fn dispatch_bytes(&self) -> usize {
        self.dispatch.bytes()
    }

    #[cfg(any(test, feature = "profile"))]
    pub(crate) const fn cached_register_count(&self) -> usize {
        self.cache.count()
    }

    #[cfg(feature = "profile")]
    pub(crate) const fn cached_guest_registers(&self) -> [u8; MAX_CACHED_REGISTERS] {
        self.cache.guests()
    }

    #[cfg(feature = "profile")]
    pub(crate) const fn hot_code_bytes(&self) -> usize {
        self.hot_code_bytes
    }

    #[cfg(feature = "profile")]
    pub(crate) const fn cold_code_bytes(&self) -> usize {
        self.cold_code_bytes
    }

    #[cfg(feature = "profile")]
    pub(crate) const fn external_thunk_bytes(&self) -> usize {
        self.external_thunk_bytes
    }

    #[cfg(feature = "profile")]
    pub(crate) const fn shared_prologue_bytes(&self) -> usize {
        self.shared_prologue_bytes
    }

    #[cfg(feature = "profile")]
    pub(crate) const fn exit_trampoline_bytes(&self) -> usize {
        self.exit_trampoline_bytes
    }
}

#[derive(Clone, Copy)]
pub(crate) struct LinkedEntry<'a> {
    program: &'a LinkedProgram,
    metadata: EntryMetadata,
}

impl LinkedEntry<'_> {
    pub(crate) fn execute(
        self,
        registers: &mut [u32; 32],
        memory: &mut rv32vm_rust_common::memory::Memory,
        pc: u32,
        remaining: u64,
    ) -> NativeRun {
        let direct_memory = memory.direct_memory();
        self.execute_inner(registers, &direct_memory, pc, remaining)
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    fn execute_inner(
        self,
        registers: &mut [u32; 32],
        direct_memory: &DirectMemory<'_>,
        pc: u32,
        remaining: u64,
    ) -> NativeRun {
        use std::mem;

        type Entry = unsafe extern "C" fn(*mut RunContext);

        debug_assert!(self.metadata.external_offset < self.program.memory.len());
        // SAFETY: The entry offset was recorded before this still-live mapping
        // was published and points at an ENDBR64-prefixed private-ABI stub.
        let address = unsafe {
            self.program
                .memory
                .address()
                .add(self.metadata.external_offset)
        };
        debug_assert_eq!(size_of::<Entry>(), size_of::<*const u8>());
        // SAFETY: `address` names finalized bytes emitted for `Entry`.
        let entry = unsafe { mem::transmute::<*const u8, Entry>(address) };
        let mut context = RunContext::new(
            registers.as_mut_ptr(),
            remaining,
            pc,
            direct_memory,
            self.program.dispatch.roots_ptr(),
            self.program.memory.address(),
        );
        // SAFETY: The mapping is RX and live, context/register borrows are
        // exclusive for the synchronous call, and every emitted path balances
        // its generated stack frame and restores all SysV callee-saved cache
        // registers before returning.
        unsafe { entry(&mut context) };
        debug_assert!(context.remaining <= remaining);
        let stop = match context.exit {
            EXIT_MISSING => NativeStop::MissingSuccessor,
            EXIT_BUDGET => NativeStop::Budget,
            EXIT_INTERPRET_ONE => NativeStop::InterpretOne,
            _ => unreachable!("linked code returned an invalid exit reason"),
        };
        NativeRun {
            pc: context.pc,
            retired: remaining - context.remaining,
            stop,
            #[cfg(feature = "profile")]
            profile: NativeRunProfile {
                blocks: context.blocks,
                direct_links: context.direct_links,
                indirect_hits: context.indirect_hits,
                indirect_misses: context.indirect_misses,
                register_loads: context.register_loads,
                register_stores: context.register_stores,
                cache_fills: self.program.cache.count() as u64,
                cache_spills: self.program.cache.count() as u64,
                cache_read_hits: context.cache_read_hits,
                cache_write_hits: context.cache_write_hits,
                fallthrough_blocks: context.fallthrough_blocks,
                branch_blocks: context.branch_blocks,
                jump_blocks: context.jump_blocks,
                memory_loads: context.memory_loads,
                memory_stores: context.memory_stores,
                direct_immediate: context.direct_immediate,
                direct_register: context.direct_register,
                direct_branch: context.direct_branch,
                direct_memory_load: context.direct_memory_load,
                direct_memory_store: context.direct_memory_store,
            },
        }
    }

    #[cfg(not(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    )))]
    fn execute_inner(
        self,
        _registers: &mut [u32; 32],
        _direct_memory: &DirectMemory<'_>,
        _pc: u32,
        _remaining: u64,
    ) -> NativeRun {
        unreachable!("linked native entries require x86-64 Linux")
    }
}
