//! Owns image-scoped blocks and their tiering state.

use std::num::NonZeroU32;

use rv32vm_rust_common::{
    machine::Machine,
    memory::{PAGE_COUNT, PAGE_SHIFT, PAGE_SIZE},
};
use rv32vm_rust_x86_block_compiler::{CompiledBlock, NativeBlock};

use crate::block::BasicBlock;

const INSTRUCTIONS_PER_PAGE: usize = PAGE_SIZE / 4;
const MAX_BLOCKS: usize = 8_192;
const MAX_DECODED_INSTRUCTIONS: usize = 262_144;
const COMPILATION_THRESHOLD: u8 = 3;
/// Shortest native prefix worth publishing for the lazy JIT.
const MIN_NATIVE_INSTRUCTIONS: usize = 2;

#[derive(Clone, Copy)]
struct BlockId(NonZeroU32);

impl BlockId {
    fn new(index: usize) -> Self {
        let value = u32::try_from(index + 1).expect("cache limit fits in u32");
        Self(NonZeroU32::new(value).expect("block IDs are one-based"))
    }

    const fn index(self) -> usize {
        (self.0.get() - 1) as usize
    }
}

struct BlockPage {
    entries: [Option<BlockId>; INSTRUCTIONS_PER_PAGE],
}

impl BlockPage {
    fn new() -> Self {
        Self {
            entries: [None; INSTRUCTIONS_PER_PAGE],
        }
    }
}

enum Tier {
    Profiling(u8),
    Native(NativeBlock),
    Disabled,
}

/// A decoded block together with its profiling or native state.
pub(crate) struct CachedBlock {
    block: BasicBlock,
    tier: Tier,
}

impl CachedBlock {
    fn new(block: BasicBlock) -> Self {
        Self {
            block,
            tier: Tier::Profiling(0),
        }
    }

    pub(crate) fn block(&self) -> &BasicBlock {
        &self.block
    }

    pub(crate) fn native(&self) -> Option<&NativeBlock> {
        match &self.tier {
            Tier::Native(native) => Some(native),
            Tier::Profiling(_) | Tier::Disabled => None,
        }
    }

    pub(crate) fn observe_and_compile(&mut self, code_budget: usize) -> usize {
        let Tier::Profiling(executions) = &mut self.tier else {
            return 0;
        };
        *executions = executions.saturating_add(1);
        if *executions < COMPILATION_THRESHOLD {
            return 0;
        }

        self.tier = CompiledBlock::compile(self.block.instructions())
            .filter(|block| block.instruction_count() >= MIN_NATIVE_INSTRUCTIONS)
            .and_then(|block| NativeBlock::publish(block, code_budget))
            .map_or(Tier::Disabled, Tier::Native);
        self.native().map_or(0, NativeBlock::mapped_len)
    }
}

pub(crate) enum BlockLookup<'a> {
    Cached(&'a mut CachedBlock),
    Transient(BasicBlock),
}

/// A bounded sparse cache indexed by guest program counter.
pub(crate) struct BlockCache {
    pages: Box<[Option<Box<BlockPage>>]>,
    blocks: Vec<CachedBlock>,
    decoded_instructions: usize,
    maximum_blocks: usize,
    maximum_decoded_instructions: usize,
}

impl Default for BlockCache {
    fn default() -> Self {
        Self {
            pages: std::iter::repeat_with(|| None).take(PAGE_COUNT).collect(),
            blocks: Vec::new(),
            decoded_instructions: 0,
            maximum_blocks: MAX_BLOCKS,
            maximum_decoded_instructions: MAX_DECODED_INSTRUCTIONS,
        }
    }
}

impl BlockCache {
    #[cfg(test)]
    fn with_limits(maximum_blocks: usize, maximum_decoded_instructions: usize) -> Self {
        Self {
            maximum_blocks,
            maximum_decoded_instructions,
            ..Self::default()
        }
    }

    pub(crate) fn clear(&mut self) {
        for page in &mut self.pages {
            *page = None;
        }
        self.blocks.clear();
        self.decoded_instructions = 0;
    }

    pub(crate) fn get_or_translate(&mut self, machine: &Machine, pc: u32) -> BlockLookup<'_> {
        let page = (pc >> PAGE_SHIFT) as usize;
        let slot = (pc as usize & (PAGE_SIZE - 1)) / 4;
        if let Some(id) = self.pages[page]
            .as_ref()
            .and_then(|page| page.entries[slot])
        {
            return BlockLookup::Cached(&mut self.blocks[id.index()]);
        }

        let block = BasicBlock::translate(machine, pc);
        let decoded = self
            .decoded_instructions
            .checked_add(block.instructions().len());
        if self.blocks.len() == self.maximum_blocks
            || decoded.is_none_or(|count| count > self.maximum_decoded_instructions)
        {
            return BlockLookup::Transient(block);
        }

        self.decoded_instructions = decoded.expect("cache limit prevents overflow");
        let id = BlockId::new(self.blocks.len());
        self.blocks.push(CachedBlock::new(block));
        let page = self.pages[page].get_or_insert_with(|| Box::new(BlockPage::new()));
        page.entries[slot] = Some(id);
        BlockLookup::Cached(&mut self.blocks[id.index()])
    }

    #[cfg(test)]
    pub(crate) fn block_count(&self) -> usize {
        self.blocks.len()
    }

    #[cfg(all(
        test,
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    pub(crate) fn native_block_count(&self) -> usize {
        self.blocks
            .iter()
            .filter(|cached| cached.native().is_some())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::memory::IMAGE_START;

    use super::{BlockCache, BlockLookup, CachedBlock, Tier};
    use crate::{
        block::BasicBlock,
        test_support::{NOP, addi, lw, machine_with_code_at},
    };

    #[test]
    fn attempts_compilation_after_three_completed_executions() {
        let machine = machine_with_code_at(&[addi(5, 0, 1), lw(6, 0, 0)], IMAGE_START);
        let mut cached = CachedBlock::new(BasicBlock::translate(&machine, IMAGE_START));

        assert_eq!(cached.observe_and_compile(usize::MAX), 0);
        assert!(matches!(cached.tier, Tier::Profiling(1)));
        assert_eq!(cached.observe_and_compile(usize::MAX), 0);
        assert!(matches!(cached.tier, Tier::Profiling(2)));
        assert_eq!(cached.observe_and_compile(usize::MAX), 0);
        assert!(matches!(cached.tier, Tier::Disabled));
    }

    #[test]
    fn full_cache_uses_transient_blocks_without_growing() {
        let machine = machine_with_code_at(&[NOP; 65], IMAGE_START);
        let mut cache = BlockCache::with_limits(1, usize::MAX);

        assert!(matches!(
            cache.get_or_translate(&machine, IMAGE_START),
            BlockLookup::Cached(_)
        ));
        assert!(matches!(
            cache.get_or_translate(&machine, IMAGE_START + 4),
            BlockLookup::Transient(_)
        ));
        assert_eq!(cache.block_count(), 1);
    }

    #[test]
    fn decoded_instruction_limit_uses_transient_blocks() {
        let machine = machine_with_code_at(&[lw(5, 0, 0), NOP], IMAGE_START);
        let mut cache = BlockCache::with_limits(usize::MAX, 1);

        assert!(matches!(
            cache.get_or_translate(&machine, IMAGE_START),
            BlockLookup::Cached(_)
        ));
        assert!(matches!(
            cache.get_or_translate(&machine, IMAGE_START + 4),
            BlockLookup::Transient(_)
        ));
        assert_eq!(cache.block_count(), 1);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn zero_code_budget_disables_native_compilation() {
        let machine =
            machine_with_code_at(&[addi(5, 0, 1), addi(5, 5, 1), lw(6, 0, 0)], IMAGE_START);
        let mut cached = CachedBlock::new(BasicBlock::translate(&machine, IMAGE_START));

        cached.observe_and_compile(0);
        cached.observe_and_compile(0);
        cached.observe_and_compile(0);

        assert!(matches!(cached.tier, Tier::Disabled));
    }
}
