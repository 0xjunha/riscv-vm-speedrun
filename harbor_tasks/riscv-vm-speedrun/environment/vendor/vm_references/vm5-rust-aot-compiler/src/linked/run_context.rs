//! Generated-code ABI layout and its single-source-of-truth field offsets.

use rv32vm_rust_common::memory::DirectMemory;

#[repr(C)]
pub(super) struct RunContext {
    pub(super) registers: *mut u32,
    pub(super) remaining: u64,
    pub(super) pc: u32,
    pub(super) exit: u32,
    pub(super) permissions: *const u8,
    pub(super) address_space: *mut u8,
    pub(super) dispatch_pages: *const usize,
    pub(super) code_base: *const u8,
    #[cfg(feature = "profile")]
    pub(super) blocks: u64,
    #[cfg(feature = "profile")]
    pub(super) direct_links: u64,
    #[cfg(feature = "profile")]
    pub(super) indirect_hits: u64,
    #[cfg(feature = "profile")]
    pub(super) indirect_misses: u64,
    #[cfg(feature = "profile")]
    pub(super) register_loads: u64,
    #[cfg(feature = "profile")]
    pub(super) register_stores: u64,
    #[cfg(feature = "profile")]
    pub(super) cache_read_hits: u64,
    #[cfg(feature = "profile")]
    pub(super) cache_write_hits: u64,
    #[cfg(feature = "profile")]
    pub(super) fallthrough_blocks: u64,
    #[cfg(feature = "profile")]
    pub(super) branch_blocks: u64,
    #[cfg(feature = "profile")]
    pub(super) jump_blocks: u64,
    #[cfg(feature = "profile")]
    pub(super) memory_loads: u64,
    #[cfg(feature = "profile")]
    pub(super) memory_stores: u64,
    #[cfg(feature = "profile")]
    pub(super) direct_immediate: u64,
    #[cfg(feature = "profile")]
    pub(super) direct_register: u64,
    #[cfg(feature = "profile")]
    pub(super) direct_branch: u64,
    #[cfg(feature = "profile")]
    pub(super) direct_memory_load: u64,
    #[cfg(feature = "profile")]
    pub(super) direct_memory_store: u64,
}

const fn disp8_offset(offset: usize) -> u8 {
    assert!(offset <= i8::MAX as usize);
    offset as u8
}

pub(super) const REGISTERS_OFFSET: u8 = disp8_offset(std::mem::offset_of!(RunContext, registers));
pub(super) const REMAINING_OFFSET: u8 = disp8_offset(std::mem::offset_of!(RunContext, remaining));
pub(super) const PC_OFFSET: u8 = disp8_offset(std::mem::offset_of!(RunContext, pc));
pub(super) const EXIT_OFFSET: u8 = disp8_offset(std::mem::offset_of!(RunContext, exit));
pub(super) const PERMISSIONS_OFFSET: u8 =
    disp8_offset(std::mem::offset_of!(RunContext, permissions));
pub(super) const ADDRESS_SPACE_OFFSET: u8 =
    disp8_offset(std::mem::offset_of!(RunContext, address_space));
pub(super) const DISPATCH_PAGES_OFFSET: u8 =
    disp8_offset(std::mem::offset_of!(RunContext, dispatch_pages));
pub(super) const CODE_BASE_OFFSET: u8 = disp8_offset(std::mem::offset_of!(RunContext, code_base));

#[cfg(feature = "profile")]
pub(super) const PROFILE_BLOCKS_OFFSET: usize = std::mem::offset_of!(RunContext, blocks);
#[cfg(feature = "profile")]
pub(super) const PROFILE_DIRECT_LINKS_OFFSET: usize =
    std::mem::offset_of!(RunContext, direct_links);
#[cfg(feature = "profile")]
pub(super) const PROFILE_INDIRECT_HITS_OFFSET: usize =
    std::mem::offset_of!(RunContext, indirect_hits);
#[cfg(feature = "profile")]
pub(super) const PROFILE_INDIRECT_MISSES_OFFSET: usize =
    std::mem::offset_of!(RunContext, indirect_misses);
#[cfg(feature = "profile")]
pub(super) const PROFILE_REGISTER_LOADS_OFFSET: usize =
    std::mem::offset_of!(RunContext, register_loads);
#[cfg(feature = "profile")]
pub(super) const PROFILE_REGISTER_STORES_OFFSET: usize =
    std::mem::offset_of!(RunContext, register_stores);
#[cfg(feature = "profile")]
pub(super) const PROFILE_CACHE_READ_HITS_OFFSET: usize =
    std::mem::offset_of!(RunContext, cache_read_hits);
#[cfg(feature = "profile")]
pub(super) const PROFILE_CACHE_WRITE_HITS_OFFSET: usize =
    std::mem::offset_of!(RunContext, cache_write_hits);
#[cfg(feature = "profile")]
pub(super) const PROFILE_FALLTHROUGH_OFFSET: usize =
    std::mem::offset_of!(RunContext, fallthrough_blocks);
#[cfg(feature = "profile")]
pub(super) const PROFILE_BRANCH_OFFSET: usize = std::mem::offset_of!(RunContext, branch_blocks);
#[cfg(feature = "profile")]
pub(super) const PROFILE_JUMP_OFFSET: usize = std::mem::offset_of!(RunContext, jump_blocks);
#[cfg(feature = "profile")]
pub(super) const PROFILE_MEMORY_LOADS_OFFSET: usize =
    std::mem::offset_of!(RunContext, memory_loads);
#[cfg(feature = "profile")]
pub(super) const PROFILE_MEMORY_STORES_OFFSET: usize =
    std::mem::offset_of!(RunContext, memory_stores);
#[cfg(feature = "profile")]
pub(super) const PROFILE_DIRECT_IMMEDIATE_OFFSET: usize =
    std::mem::offset_of!(RunContext, direct_immediate);
#[cfg(feature = "profile")]
pub(super) const PROFILE_DIRECT_REGISTER_OFFSET: usize =
    std::mem::offset_of!(RunContext, direct_register);
#[cfg(feature = "profile")]
pub(super) const PROFILE_DIRECT_BRANCH_OFFSET: usize =
    std::mem::offset_of!(RunContext, direct_branch);
#[cfg(feature = "profile")]
pub(super) const PROFILE_DIRECT_MEMORY_LOAD_OFFSET: usize =
    std::mem::offset_of!(RunContext, direct_memory_load);
#[cfg(feature = "profile")]
pub(super) const PROFILE_DIRECT_MEMORY_STORE_OFFSET: usize =
    std::mem::offset_of!(RunContext, direct_memory_store);

impl RunContext {
    pub(super) fn new(
        registers: *mut u32,
        remaining: u64,
        pc: u32,
        direct_memory: &DirectMemory<'_>,
        dispatch_pages: *const usize,
        code_base: *const u8,
    ) -> Self {
        Self {
            registers,
            remaining,
            pc,
            exit: 0,
            permissions: direct_memory.permissions_ptr(),
            address_space: direct_memory.address_space_ptr(),
            dispatch_pages,
            code_base,
            #[cfg(feature = "profile")]
            blocks: 0,
            #[cfg(feature = "profile")]
            direct_links: 0,
            #[cfg(feature = "profile")]
            indirect_hits: 0,
            #[cfg(feature = "profile")]
            indirect_misses: 0,
            #[cfg(feature = "profile")]
            register_loads: 0,
            #[cfg(feature = "profile")]
            register_stores: 0,
            #[cfg(feature = "profile")]
            cache_read_hits: 0,
            #[cfg(feature = "profile")]
            cache_write_hits: 0,
            #[cfg(feature = "profile")]
            fallthrough_blocks: 0,
            #[cfg(feature = "profile")]
            branch_blocks: 0,
            #[cfg(feature = "profile")]
            jump_blocks: 0,
            #[cfg(feature = "profile")]
            memory_loads: 0,
            #[cfg(feature = "profile")]
            memory_stores: 0,
            #[cfg(feature = "profile")]
            direct_immediate: 0,
            #[cfg(feature = "profile")]
            direct_register: 0,
            #[cfg(feature = "profile")]
            direct_branch: 0,
            #[cfg(feature = "profile")]
            direct_memory_load: 0,
            #[cfg(feature = "profile")]
            direct_memory_store: 0,
        }
    }
}

const _: () = assert!(REGISTERS_OFFSET == 0);
const _: () = assert!(REMAINING_OFFSET == 8);
const _: () = assert!(PC_OFFSET == 16);
const _: () = assert!(EXIT_OFFSET == 20);
const _: () = assert!(PERMISSIONS_OFFSET == 24);
const _: () = assert!(ADDRESS_SPACE_OFFSET == 32);
const _: () = assert!(DISPATCH_PAGES_OFFSET == 40);
const _: () = assert!(CODE_BASE_OFFSET == 48);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_BLOCKS_OFFSET == 56);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_DIRECT_LINKS_OFFSET == 64);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_INDIRECT_HITS_OFFSET == 72);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_INDIRECT_MISSES_OFFSET == 80);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_REGISTER_LOADS_OFFSET == 88);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_REGISTER_STORES_OFFSET == 96);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_CACHE_READ_HITS_OFFSET == 104);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_CACHE_WRITE_HITS_OFFSET == 112);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_FALLTHROUGH_OFFSET == 120);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_BRANCH_OFFSET == 128);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_JUMP_OFFSET == 136);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_MEMORY_LOADS_OFFSET == 144);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_MEMORY_STORES_OFFSET == 152);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_DIRECT_IMMEDIATE_OFFSET == 160);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_DIRECT_REGISTER_OFFSET == 168);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_DIRECT_BRANCH_OFFSET == 176);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_DIRECT_MEMORY_LOAD_OFFSET == 184);
#[cfg(feature = "profile")]
const _: () = assert!(PROFILE_DIRECT_MEMORY_STORE_OFFSET == 192);
