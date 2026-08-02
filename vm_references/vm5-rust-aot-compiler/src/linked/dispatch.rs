//! Bounded sparse guest-PC to native-entry dispatch ownership.

use std::{collections::BTreeMap, mem::size_of};

use rv32vm_rust_common::memory::{ADDRESS_SPACE_SIZE, PAGE_COUNT, PAGE_SHIFT, PAGE_SIZE};

use super::{ENTRY_BYTES, EntryMetadata, MAX_LINKED_BLOCKS};

pub(super) const INSTRUCTIONS_PER_PAGE: usize = PAGE_SIZE / size_of::<u32>();
pub(super) const MAX_DISPATCH_BYTES: usize = PAGE_COUNT * size_of::<usize>()
    + MAX_LINKED_BLOCKS * (PAGE_SIZE + size_of::<Box<[u32; INSTRUCTIONS_PER_PAGE]>>());

/// Sparse immutable guest-PC to native-entry offsets used only by in-image
/// indirect dispatch. Leaves store offset-plus-one so zero remains a miss.
pub(super) struct DispatchTable {
    roots: Box<[usize]>,
    _leaves: Vec<Box<[u32; INSTRUCTIONS_PER_PAGE]>>,
    _entries: usize,
    _bytes: usize,
}

impl DispatchTable {
    pub(super) fn build(code: &[u8], entries: &[(u32, EntryMetadata)]) -> Option<Self> {
        if entries.is_empty() || entries.len() > MAX_LINKED_BLOCKS {
            return None;
        }
        let mut staged = BTreeMap::<usize, Box<[u32; INSTRUCTIONS_PER_PAGE]>>::new();
        for &(pc, metadata) in entries {
            if pc & 3 != 0 || pc >= ADDRESS_SPACE_SIZE {
                return None;
            }
            let end = metadata.indirect_offset.checked_add(ENTRY_BYTES.len())?;
            if code.get(metadata.indirect_offset..end)? != ENTRY_BYTES {
                return None;
            }
            let encoded = u32::try_from(metadata.indirect_offset)
                .ok()?
                .checked_add(1)?;
            let page_number = (pc >> PAGE_SHIFT) as usize;
            let slot = (pc as usize & (PAGE_SIZE - 1)) / size_of::<u32>();
            let page = staged
                .entry(page_number)
                .or_insert_with(|| Box::new([0; INSTRUCTIONS_PER_PAGE]));
            if std::mem::replace(&mut page[slot], encoded) != 0 {
                return None;
            }
        }

        let mut roots = vec![0; PAGE_COUNT].into_boxed_slice();
        let mut leaves = Vec::with_capacity(staged.len());
        for (page_number, page) in staged {
            roots[page_number] = page.as_ptr() as usize;
            leaves.push(page);
        }
        let root_bytes = roots.len().checked_mul(size_of::<usize>())?;
        let leaf_bytes = leaves.len().checked_mul(PAGE_SIZE)?;
        let owner_bytes = leaves
            .capacity()
            .checked_mul(size_of::<Box<[u32; INSTRUCTIONS_PER_PAGE]>>())?;
        let bytes = root_bytes
            .checked_add(leaf_bytes)?
            .checked_add(owner_bytes)?;
        if bytes > MAX_DISPATCH_BYTES {
            return None;
        }
        Some(Self {
            roots,
            _leaves: leaves,
            _entries: entries.len(),
            _bytes: bytes,
        })
    }

    pub(super) const fn roots_ptr(&self) -> *const usize {
        self.roots.as_ptr()
    }

    #[cfg(any(test, feature = "profile"))]
    pub(super) const fn page_count(&self) -> usize {
        self._leaves.len()
    }

    #[cfg(any(test, feature = "profile"))]
    pub(super) const fn entry_count(&self) -> usize {
        self._entries
    }

    #[cfg(any(test, feature = "profile"))]
    pub(super) const fn bytes(&self) -> usize {
        self._bytes
    }

    #[cfg(test)]
    pub(super) fn encoded_entry(&self, pc: u32) -> Option<u32> {
        if pc & 3 != 0 || pc >= ADDRESS_SPACE_SIZE {
            return None;
        }
        let page = *self.roots.get((pc >> PAGE_SHIFT) as usize)?;
        if page == 0 {
            return Some(0);
        }
        let slot = (pc as usize & (PAGE_SIZE - 1)) / size_of::<u32>();
        // SAFETY: Every nonzero root was derived from one still-owned leaf and
        // the checked slot is within that fixed-size allocation.
        Some(unsafe { *(page as *const u32).add(slot) })
    }
}
