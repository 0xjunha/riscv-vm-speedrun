use std::num::NonZeroU32;

use rv32vm_rust_common::{
    machine::Machine,
    memory::{PAGE_COUNT, PAGE_SHIFT, PAGE_SIZE},
};

use crate::block::BasicBlock;

const INSTRUCTIONS_PER_PAGE: usize = PAGE_SIZE / 4;
const MAX_BLOCKS: usize = 8_192;
const MAX_DECODED_INSTRUCTIONS: usize = 262_144;

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

pub(crate) enum BlockLookup<'a> {
    Cached(&'a BasicBlock),
    Transient(BasicBlock),
}

impl BlockLookup<'_> {
    pub(crate) fn block(&self) -> &BasicBlock {
        match self {
            Self::Cached(block) => block,
            Self::Transient(block) => block,
        }
    }
}

/// Stores decoded blocks by their starting program counter.
pub(crate) struct BlockCache {
    pages: Box<[Option<Box<BlockPage>>]>,
    blocks: Vec<BasicBlock>,
    decoded_instructions: usize,
    maximum_blocks: usize,
    maximum_decoded_instructions: usize,
    #[cfg(test)]
    translations: usize,
}

impl Default for BlockCache {
    fn default() -> Self {
        Self {
            pages: std::iter::repeat_with(|| None).take(PAGE_COUNT).collect(),
            blocks: Vec::new(),
            decoded_instructions: 0,
            maximum_blocks: MAX_BLOCKS,
            maximum_decoded_instructions: MAX_DECODED_INSTRUCTIONS,
            #[cfg(test)]
            translations: 0,
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
            return BlockLookup::Cached(&self.blocks[id.index()]);
        }

        let block = BasicBlock::translate(machine, pc);
        #[cfg(test)]
        {
            self.translations += 1;
        }
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
        self.blocks.push(block);
        let page = self.pages[page].get_or_insert_with(|| Box::new(BlockPage::new()));
        page.entries[slot] = Some(id);
        BlockLookup::Cached(&self.blocks[id.index()])
    }

    #[cfg(test)]
    pub(crate) fn translation_count(&self) -> usize {
        self.translations
    }

    #[cfg(test)]
    fn block_count(&self) -> usize {
        self.blocks.len()
    }
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::memory::IMAGE_START;

    use super::BlockCache;
    use crate::test_support::{NOP, machine_with_code_at};

    #[test]
    fn full_cache_uses_transient_blocks_without_growing() {
        let machine = machine_with_code_at(&[NOP; 65], IMAGE_START);
        let mut cache = BlockCache::with_limits(1, 64);

        cache.get_or_translate(&machine, IMAGE_START);
        cache.get_or_translate(&machine, IMAGE_START + 4);

        assert_eq!(cache.block_count(), 1);
        assert_eq!(cache.translation_count(), 2);
    }
}
