//! VM5-private x86-64 code emission and whole-image relocation.
//!
//! The surrounding private modules isolate RV32 lowering, persistent register
//! selection, indirect dispatch, the generated-code ABI, executable-memory
//! ownership, and the published program lifecycle. This root contains the
//! tightly coupled encoder and linker that turn lowered blocks into one image.

use std::collections::BTreeMap;

use rv32vm_rust_common::memory::{
    ADDRESS_SPACE_SIZE, PAGE_SHIFT, PAGE_SIZE, PERM_READ, PERM_WRITE,
};

mod dispatch;
mod executable_memory;
mod lowering;
mod program;
mod register_cache;
mod run_context;
#[cfg(test)]
mod tests;

use dispatch::DispatchTable;
use executable_memory::ExecutableMemory;
pub(crate) use lowering::{BlockFlow, BlockInstruction, Condition, LinkedBlock};
use lowering::{ImmediateOperation, Lowering, MemoryWidth, RegisterOperation};
pub(crate) use program::{LinkedEntry, LinkedProgram};
#[cfg(feature = "profile")]
pub(crate) use program::{NativeRunProfile, NativeStop};
use register_cache::{BinaryOperation32, CachedHost, Operand32, Register32, RegisterCache};
use run_context::{
    ADDRESS_SPACE_OFFSET, CODE_BASE_OFFSET, DISPATCH_PAGES_OFFSET, EXIT_OFFSET, PC_OFFSET,
    PERMISSIONS_OFFSET, REGISTERS_OFFSET, REMAINING_OFFSET,
};
#[cfg(feature = "profile")]
use run_context::{
    PROFILE_BLOCKS_OFFSET, PROFILE_BRANCH_OFFSET, PROFILE_CACHE_READ_HITS_OFFSET,
    PROFILE_CACHE_WRITE_HITS_OFFSET, PROFILE_DIRECT_BRANCH_OFFSET, PROFILE_DIRECT_IMMEDIATE_OFFSET,
    PROFILE_DIRECT_LINKS_OFFSET, PROFILE_DIRECT_MEMORY_LOAD_OFFSET,
    PROFILE_DIRECT_MEMORY_STORE_OFFSET, PROFILE_DIRECT_REGISTER_OFFSET, PROFILE_FALLTHROUGH_OFFSET,
    PROFILE_INDIRECT_HITS_OFFSET, PROFILE_INDIRECT_MISSES_OFFSET, PROFILE_JUMP_OFFSET,
    PROFILE_MEMORY_LOADS_OFFSET, PROFILE_MEMORY_STORES_OFFSET, PROFILE_REGISTER_LOADS_OFFSET,
    PROFILE_REGISTER_STORES_OFFSET,
};

const ENTRY_BYTES: [u8; 4] = [0xf3, 0x0f, 0x1e, 0xfa];
#[cfg(not(feature = "profile"))]
const EDGE_SLOT_BYTES: usize = 5;
#[cfg(feature = "profile")]
const EDGE_SLOT_BYTES: usize = 9;
const BUDGET_VENEER_BYTES: usize = 14;
const INTERPRET_ONE_VENEER_BYTES: usize = 14;
const MISSING_VENEER_BYTES: usize = 10;
#[cfg(not(feature = "profile"))]
const INDIRECT_MISSING_VENEER_BYTES: usize = 7;
#[cfg(feature = "profile")]
const INDIRECT_MISSING_VENEER_BYTES: usize = 11;
const EXTERNAL_THUNK_BYTES: usize = 16;
const MAX_SHARED_PROLOGUE_BYTES: usize = if cfg!(feature = "profile") { 58 } else { 50 };
const MAX_EXIT_TRAMPOLINE_BYTES: usize = if cfg!(feature = "profile") { 73 } else { 65 };
const MAX_FIXED_CODE_BYTES: usize = MAX_SHARED_PROLOGUE_BYTES + MAX_EXIT_TRAMPOLINE_BYTES;
const EXIT_MISSING: u32 = 1;
const EXIT_BUDGET: u32 = 2;
const EXIT_INTERPRET_ONE: u32 = 3;
const _: () = assert!(PAGE_SIZE.is_power_of_two());
const _: () = assert!(PAGE_SIZE == 1_usize << PAGE_SHIFT);
const _: () = assert!(ADDRESS_SPACE_SIZE.is_power_of_two());
const _: () = assert!(ADDRESS_SPACE_SIZE.is_multiple_of(4));
pub(crate) const MAX_LINKED_BLOCKS: usize = 8_192;
const _: () = assert!(REGISTERS_OFFSET == 0);

fn reserved_len(instructions: &[Lowering], flow: BlockFlow) -> Option<usize> {
    // An uncached body is a mapping-independent upper bound: every cached
    // register move is no larger than the corresponding [RSI+disp8] access.
    let mut emitter = Emitter::new(RegisterCache::empty());
    emitter.emit_block(instructions, flow, 0)?;
    emitter.reserved_code_len()
}

impl LinkedBlock {
    pub(crate) fn compile(instructions: &[BlockInstruction]) -> Option<Self> {
        let (pc, instructions, flow) = lowering::lower_block(instructions)?;
        let reserved_code_len = reserved_len(&instructions, flow)?;
        Some(Self {
            pc,
            instructions,
            flow,
            reserved_code_len,
        })
    }
}

const fn x86_condition(condition: Condition) -> u8 {
    match condition {
        Condition::Equal => 0x84,
        Condition::NotEqual => 0x85,
        Condition::LessThan => 0x8c,
        Condition::GreaterOrEqual => 0x8d,
        Condition::Below => 0x82,
        Condition::AboveOrEqual => 0x83,
    }
}

#[derive(Clone, Copy)]
struct EntryMetadata {
    external_offset: usize,
    indirect_offset: usize,
    hot_offset: usize,
}

struct ResolvedImage {
    code: Vec<u8>,
    entries: Vec<(u32, EntryMetadata)>,
    #[cfg(any(test, feature = "profile"))]
    hot_code_bytes: usize,
    #[cfg(any(test, feature = "profile"))]
    cold_code_bytes: usize,
    #[cfg(any(test, feature = "profile"))]
    external_thunk_bytes: usize,
    #[cfg(any(test, feature = "profile"))]
    shared_prologue_bytes: usize,
    #[cfg(any(test, feature = "profile"))]
    exit_trampoline_bytes: usize,
}

#[derive(Clone, Copy)]
struct EdgeRelocation {
    slot_offset: usize,
    target_pc: u32,
}

#[cfg(not(feature = "profile"))]
#[derive(Clone, Copy)]
struct ConditionalEdgeRelocation {
    branch: LocalFixup,
    target_pc: u32,
}

#[derive(Clone, Copy)]
struct BudgetRelocation {
    branch: LocalFixup,
    pc: u32,
    count: u8,
}

#[derive(Clone, Copy)]
struct PreciseExit {
    pc: u32,
    refund: u8,
}

struct InterpretOneRelocation {
    branches: Vec<LocalFixup>,
    pc: u32,
    refund: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LocalFixup {
    displacement_offset: usize,
    instruction_end: usize,
}

struct Emitter {
    code: Vec<u8>,
    cache: RegisterCache,
    #[cfg(not(feature = "profile"))]
    local_guest: Option<usize>,
    entries: Vec<(u32, EntryMetadata)>,
    edges: Vec<EdgeRelocation>,
    #[cfg(not(feature = "profile"))]
    conditional_edges: Vec<ConditionalEdgeRelocation>,
    budget_exits: Vec<BudgetRelocation>,
    interpret_one_exits: Vec<InterpretOneRelocation>,
    indirect_misses: Vec<LocalFixup>,
    local_fixups: Vec<LocalFixup>,
}

impl Emitter {
    fn new(cache: RegisterCache) -> Self {
        Self {
            code: Vec::new(),
            cache,
            #[cfg(not(feature = "profile"))]
            local_guest: None,
            entries: Vec::new(),
            edges: Vec::new(),
            #[cfg(not(feature = "profile"))]
            conditional_edges: Vec::new(),
            budget_exits: Vec::new(),
            interpret_one_exits: Vec::new(),
            indirect_misses: Vec::new(),
            local_fixups: Vec::new(),
        }
    }

    fn reserved_code_len(&self) -> Option<usize> {
        self.code
            .len()
            .checked_add(
                usize::from(!self.cache.is_empty())
                    .checked_mul(self.entries.len())?
                    .checked_mul(EXTERNAL_THUNK_BYTES)?,
            )?
            .checked_add(self.budget_exits.len().checked_mul(BUDGET_VENEER_BYTES)?)?
            .checked_add(
                self.interpret_one_exits
                    .len()
                    .checked_mul(INTERPRET_ONE_VENEER_BYTES)?,
            )?
            .checked_add(
                usize::from(!self.indirect_misses.is_empty())
                    .checked_mul(INDIRECT_MISSING_VENEER_BYTES)?,
            )?
            .checked_add(self.conditional_missing_bytes()?)?
            .checked_add(self.edges.len().checked_mul(MISSING_VENEER_BYTES)?)
    }

    const fn conditional_missing_bytes(&self) -> Option<usize> {
        #[cfg(not(feature = "profile"))]
        {
            self.conditional_edges
                .len()
                .checked_mul(MISSING_VENEER_BYTES)
        }
        #[cfg(feature = "profile")]
        {
            Some(0)
        }
    }

    fn emit_block(&mut self, instructions: &[Lowering], flow: BlockFlow, pc: u32) -> Option<()> {
        self.select_local_cache(instructions, flow);
        let external_offset = if self.cache.is_empty() {
            // With no cache to preserve, inline external entry is smaller than
            // a cold thunk plus a shared prologue. It also retains the old
            // straight-line path for short and infrequently invoked images.
            let external_offset = self.code.len();
            self.code.extend_from_slice(&ENTRY_BYTES);
            self.code.extend_from_slice(&[0x48, 0x8b, 0x37]); // mov rsi, [rdi+registers]
            self.code
                .extend_from_slice(&[0x4c, 0x8b, 0x47, PERMISSIONS_OFFSET]);
            self.code
                .extend_from_slice(&[0x4c, 0x8b, 0x4f, ADDRESS_SPACE_OFFSET]);
            self.code
                .extend_from_slice(&[0x4c, 0x8b, 0x57, REMAINING_OFFSET]);
            external_offset
        } else {
            // Cached images materialize external thunks together in the cold
            // section during resolve and enter one shared cache prologue.
            usize::MAX
        };
        // A cached block laid out immediately after its direct predecessor can
        // put the JALR landing pad in the predecessor's reserved edge slot.
        // Direct execution crosses that pad and dispatch still lands on CET.
        let overlapping_entry = self
            .edges
            .last()
            .filter(|edge| !self.cache.is_empty() && edge.target_pc == pc)
            .and_then(|edge| edge.slot_offset.checked_add(EDGE_SLOT_BYTES))
            .filter(|&edge_end| edge_end == self.code.len())
            .and_then(|edge_end| edge_end.checked_sub(ENTRY_BYTES.len()));
        let indirect_offset = if let Some(offset) = overlapping_entry {
            self.code.get_mut(offset..)?.copy_from_slice(&ENTRY_BYTES);
            offset
        } else {
            let offset = self.code.len();
            self.code.extend_from_slice(&ENTRY_BYTES);
            offset
        };
        let hot_offset = self.code.len();
        self.entries.push((
            pc,
            EntryMetadata {
                external_offset,
                indirect_offset,
                hot_offset,
            },
        ));

        let count = u8::try_from(instructions.len()).ok()?;
        if count == 0 || count > 64 {
            return None;
        }
        // Reserve the entire non-trapping block before any guest-visible
        // effect. Unsigned underflow branches to a cold veneer which restores
        // R10 before returning the untouched block for one-step fallback.
        self.code.extend_from_slice(&[0x49, 0x83, 0xea, count]); // sub r10, count
        let branch = self.cold_jcc(0x82)?; // jb budget exit
        self.budget_exits
            .push(BudgetRelocation { branch, pc, count });
        self.fill_local_cache();
        #[cfg(feature = "profile")]
        self.profile_block(instructions)?;

        let mut local_dirty = false;
        for (index, instruction) in instructions.iter().enumerate() {
            let refund = count.checked_sub(u8::try_from(index).ok()?)?;
            if matches!(instruction, Lowering::Load { .. } | Lowering::Store { .. }) && local_dirty
            {
                self.spill_local_cache();
                local_dirty = false;
            }
            let writes_local = self.writes_local(*instruction);
            match *instruction {
                Lowering::WriteImmediate { destination, value } => {
                    self.write_immediate(destination, value);
                }
                Lowering::Jump {
                    destination,
                    link,
                    target,
                } => {
                    #[cfg(feature = "profile")]
                    self.increment_context(PROFILE_JUMP_OFFSET);
                    self.write_immediate(destination, link);
                    local_dirty |= writes_local;
                    if local_dirty {
                        self.spill_local_cache();
                    }
                    self.edge_slot(target)?;
                    return Some(());
                }
                Lowering::IndirectJump {
                    pc,
                    destination,
                    source,
                    immediate,
                    link,
                } => {
                    self.indirect_jump(pc, destination, source, immediate, link)?;
                    return Some(());
                }
                Lowering::Branch {
                    left,
                    right,
                    condition,
                    fallthrough,
                    target,
                } => {
                    #[cfg(feature = "profile")]
                    self.increment_context(PROFILE_BRANCH_OFFSET);
                    if local_dirty {
                        self.spill_local_cache();
                    }
                    self.branch(left, right, condition, fallthrough, target)?;
                    return Some(());
                }
                Lowering::Immediate {
                    destination,
                    source,
                    operation,
                } => self.immediate(destination, source, operation),
                Lowering::Register {
                    destination,
                    left,
                    right,
                    operation,
                } => self.register(destination, left, right, operation)?,
                Lowering::Load {
                    pc,
                    destination,
                    base,
                    immediate,
                    width,
                    signed,
                } => {
                    self.checked_load(
                        PreciseExit { pc, refund },
                        destination,
                        base,
                        immediate,
                        width,
                        signed,
                    )?;
                }
                Lowering::Store {
                    pc,
                    base,
                    source,
                    immediate,
                    width,
                } => {
                    self.checked_store(PreciseExit { pc, refund }, base, source, immediate, width)?;
                }
                Lowering::Fence => {}
            }
            local_dirty |= writes_local;
        }

        if matches!(flow, BlockFlow::Fallthrough { .. }) {
            let [Some(pc), None] = flow.successors() else {
                return None;
            };
            #[cfg(feature = "profile")]
            self.increment_context(PROFILE_FALLTHROUGH_OFFSET);
            if local_dirty {
                self.spill_local_cache();
            }
            self.edge_slot(pc)?;
        }
        Some(())
    }

    #[cfg(not(feature = "profile"))]
    fn select_local_cache(&mut self, instructions: &[Lowering], flow: BlockFlow) {
        self.local_guest = None;
        if matches!(flow, BlockFlow::IndirectJump { .. }) {
            return;
        }

        let mut scores = [0; 32];
        let mut accesses = [0; 32];
        for &instruction in instructions {
            instruction.score_register_uses(&mut scores, &mut accesses, 1);
        }
        let mut best = None;
        for guest in 1..32 {
            if self.cache.host(guest).is_some() {
                continue;
            }
            let writes = accesses[guest]
                .saturating_mul(2)
                .saturating_sub(scores[guest]);
            let overhead = 1 + u64::from(writes != 0);
            let savings = accesses[guest].saturating_sub(overhead);
            if savings < 2 {
                continue;
            }
            let rank = (savings, scores[guest], usize::MAX - guest);
            if best.is_none_or(|(best_rank, _)| rank > best_rank) {
                best = Some((rank, guest));
            }
        }
        self.local_guest = best.map(|(_, guest)| guest);
    }

    #[cfg(feature = "profile")]
    const fn select_local_cache(&mut self, _instructions: &[Lowering], _flow: BlockFlow) {}

    #[cfg(not(feature = "profile"))]
    fn fill_local_cache(&mut self) {
        if let Some(guest) = self.local_guest {
            self.code
                .extend_from_slice(&[0x44, 0x8b, 0x5e, register_offset(guest)]);
        }
    }

    #[cfg(feature = "profile")]
    const fn fill_local_cache(&mut self) {}

    #[cfg(not(feature = "profile"))]
    fn spill_local_cache(&mut self) {
        if let Some(guest) = self.local_guest {
            self.code
                .extend_from_slice(&[0x44, 0x89, 0x5e, register_offset(guest)]);
        }
    }

    #[cfg(feature = "profile")]
    const fn spill_local_cache(&mut self) {}

    #[cfg(not(feature = "profile"))]
    fn writes_local(&self, instruction: Lowering) -> bool {
        self.local_guest
            .is_some_and(|guest| instruction.writes_register(guest))
    }

    #[cfg(feature = "profile")]
    const fn writes_local(&self, _instruction: Lowering) -> bool {
        false
    }

    #[cfg(feature = "profile")]
    fn profile_block(&mut self, instructions: &[Lowering]) -> Option<()> {
        self.increment_context(PROFILE_BLOCKS_OFFSET);

        // These counters describe dynamic executions of the new generic
        // direct-operand lowering families. Emit every ADD, including a zero
        // value, so cached and uncached mappings have identical admission
        // size even when only a cached checked-memory operand is direct.
        let mut direct_immediate = 0;
        let mut direct_register = 0;
        let mut direct_branch = 0;
        let mut direct_memory_load = 0;
        let mut direct_memory_store = 0;
        for &instruction in instructions {
            match instruction {
                Lowering::Immediate {
                    operation:
                        ImmediateOperation::Add(_)
                        | ImmediateOperation::Xor(_)
                        | ImmediateOperation::Or(_)
                        | ImmediateOperation::And(_)
                        | ImmediateOperation::ShiftLeft(_)
                        | ImmediateOperation::ShiftRight(_)
                        | ImmediateOperation::ShiftRightArithmetic(_),
                    ..
                } => {
                    direct_immediate += 1;
                }
                Lowering::Register {
                    operation:
                        RegisterOperation::Add
                        | RegisterOperation::Subtract
                        | RegisterOperation::Xor
                        | RegisterOperation::Or
                        | RegisterOperation::And
                        | RegisterOperation::Multiply,
                    ..
                } => {
                    direct_register += 1;
                }
                Lowering::Branch { .. } => direct_branch += 1,
                Lowering::Load { destination, .. } if self.cache.host(destination).is_some() => {
                    direct_memory_load += 1;
                }
                Lowering::Store { source, .. } if self.cache.host(source).is_some() => {
                    direct_memory_store += 1;
                }
                _ => {}
            }
        }
        self.add_context_exact(PROFILE_DIRECT_IMMEDIATE_OFFSET, direct_immediate)?;
        self.add_context_exact(PROFILE_DIRECT_REGISTER_OFFSET, direct_register)?;
        self.add_context_exact(PROFILE_DIRECT_BRANCH_OFFSET, direct_branch)?;
        self.add_context_exact(PROFILE_DIRECT_MEMORY_LOAD_OFFSET, direct_memory_load)?;
        self.add_context_exact(PROFILE_DIRECT_MEMORY_STORE_OFFSET, direct_memory_store)?;
        Some(())
    }

    #[cfg(feature = "profile")]
    fn increment_context(&mut self, offset: usize) {
        if let Some(offset) = u8::try_from(offset)
            .ok()
            .filter(|offset| *offset <= i8::MAX as u8)
        {
            self.code.extend_from_slice(&[0x48, 0xff, 0x47, offset]);
        } else {
            self.code.extend_from_slice(&[0x48, 0xff, 0x87]);
            self.code.extend_from_slice(
                &u32::try_from(offset)
                    .expect("RunContext profile offset fits in u32")
                    .to_le_bytes(),
            );
        }
    }

    #[cfg(feature = "profile")]
    fn add_context(&mut self, offset: usize, value: usize) -> Option<()> {
        if value == 0 {
            return Some(());
        }
        self.add_context_exact(offset, value)
    }

    #[cfg(feature = "profile")]
    fn add_context_exact(&mut self, offset: usize, value: usize) -> Option<()> {
        if let Some(offset) = u8::try_from(offset)
            .ok()
            .filter(|offset| *offset <= i8::MAX as u8)
        {
            self.code.extend_from_slice(&[0x48, 0x81, 0x47, offset]);
        } else {
            self.code.extend_from_slice(&[0x48, 0x81, 0x87]);
            self.code
                .extend_from_slice(&u32::try_from(offset).ok()?.to_le_bytes());
        }
        self.code
            .extend_from_slice(&u32::try_from(value).ok()?.to_le_bytes());
        Some(())
    }

    fn emit_external_thunks_and_prologue(&mut self) -> Option<(usize, usize)> {
        if self.cache.is_empty() {
            debug_assert!(
                self.entries
                    .iter()
                    .all(|(_, entry)| entry.external_offset != usize::MAX)
            );
            return Some((0, 0));
        }

        let thunks_start = self.code.len();
        let mut prologue_jumps = Vec::with_capacity(self.entries.len());
        for index in 0..self.entries.len() {
            let thunk_start = self.code.len();
            let indirect_offset = self.entries[index].1.indirect_offset;
            self.entries[index].1.external_offset = thunk_start;
            self.code.extend_from_slice(&ENTRY_BYTES);

            // lea r11, [rip + rel32] identifies this block's internal ENDBR64
            // without consuming a cache or anchor register.
            let lea_start = self.code.len();
            self.code.extend_from_slice(&[0x4c, 0x8d, 0x1d, 0, 0, 0, 0]);
            patch_relative(
                &mut self.code,
                LocalFixup {
                    displacement_offset: lea_start.checked_add(3)?,
                    instruction_end: lea_start.checked_add(7)?,
                },
                indirect_offset,
            )?;
            prologue_jumps.push(self.cold_jump()?);
            if self.code.len().checked_sub(thunk_start)? != EXTERNAL_THUNK_BYTES {
                return None;
            }
        }
        let external_thunk_bytes = self.code.len().checked_sub(thunks_start)?;

        let prologue_start = self.code.len();
        for jump in prologue_jumps {
            patch_relative(&mut self.code, jump, prologue_start)?;
        }
        for (host, _) in self.cache.entries() {
            self.push_host(host);
        }
        self.code.extend_from_slice(&[0x48, 0x8b, 0x37]); // mov rsi, [rdi+registers]
        self.code
            .extend_from_slice(&[0x4c, 0x8b, 0x47, PERMISSIONS_OFFSET]);
        self.code
            .extend_from_slice(&[0x4c, 0x8b, 0x4f, ADDRESS_SPACE_OFFSET]);
        self.code
            .extend_from_slice(&[0x4c, 0x8b, 0x57, REMAINING_OFFSET]);
        for (host, guest) in self.cache.entries() {
            self.fill_host(host, guest);
        }
        #[cfg(feature = "profile")]
        self.add_context(PROFILE_REGISTER_LOADS_OFFSET, self.cache.count())?;
        // R11 names an internal ENDBR64 and is therefore a valid CET indirect
        // branch target. The landing pad preserves all live private-ABI state.
        self.code.extend_from_slice(&[0x41, 0xff, 0xe3]); // jmp r11
        let shared_prologue_bytes = self.code.len().checked_sub(prologue_start)?;
        (shared_prologue_bytes <= MAX_SHARED_PROLOGUE_BYTES)
            .then_some((external_thunk_bytes, shared_prologue_bytes))
    }

    fn push_host(&mut self, host: CachedHost) {
        match host {
            CachedHost::Ebx => self.code.push(0x53),
            CachedHost::Ebp => self.code.push(0x55),
            CachedHost::R12d => self.code.extend_from_slice(&[0x41, 0x54]),
            CachedHost::R13d => self.code.extend_from_slice(&[0x41, 0x55]),
            CachedHost::R14d => self.code.extend_from_slice(&[0x41, 0x56]),
            CachedHost::R15d => self.code.extend_from_slice(&[0x41, 0x57]),
        }
    }

    fn pop_host(&mut self, host: CachedHost) {
        match host {
            CachedHost::Ebx => self.code.push(0x5b),
            CachedHost::Ebp => self.code.push(0x5d),
            CachedHost::R12d => self.code.extend_from_slice(&[0x41, 0x5c]),
            CachedHost::R13d => self.code.extend_from_slice(&[0x41, 0x5d]),
            CachedHost::R14d => self.code.extend_from_slice(&[0x41, 0x5e]),
            CachedHost::R15d => self.code.extend_from_slice(&[0x41, 0x5f]),
        }
    }

    fn fill_host(&mut self, host: CachedHost, guest: usize) {
        let offset = register_offset(guest);
        match host {
            CachedHost::Ebx => self.code.extend_from_slice(&[0x8b, 0x5e, offset]),
            CachedHost::Ebp => self.code.extend_from_slice(&[0x8b, 0x6e, offset]),
            CachedHost::R12d => self.code.extend_from_slice(&[0x44, 0x8b, 0x66, offset]),
            CachedHost::R13d => self.code.extend_from_slice(&[0x44, 0x8b, 0x6e, offset]),
            CachedHost::R14d => self.code.extend_from_slice(&[0x44, 0x8b, 0x76, offset]),
            CachedHost::R15d => self.code.extend_from_slice(&[0x44, 0x8b, 0x7e, offset]),
        }
    }

    fn spill_host(&mut self, host: CachedHost, guest: usize) {
        let offset = register_offset(guest);
        match host {
            CachedHost::Ebx => self.code.extend_from_slice(&[0x89, 0x5e, offset]),
            CachedHost::Ebp => self.code.extend_from_slice(&[0x89, 0x6e, offset]),
            CachedHost::R12d => self.code.extend_from_slice(&[0x44, 0x89, 0x66, offset]),
            CachedHost::R13d => self.code.extend_from_slice(&[0x44, 0x89, 0x6e, offset]),
            CachedHost::R14d => self.code.extend_from_slice(&[0x44, 0x89, 0x76, offset]),
            CachedHost::R15d => self.code.extend_from_slice(&[0x44, 0x89, 0x7e, offset]),
        }
    }

    fn host_register(&self, guest: usize) -> Option<Register32> {
        self.cache.host(guest).map(CachedHost::register).or({
            #[cfg(not(feature = "profile"))]
            {
                (self.local_guest == Some(guest)).then_some(Register32::R11d)
            }
            #[cfg(feature = "profile")]
            {
                None
            }
        })
    }

    fn guest_operand(&self, register: usize) -> Option<Operand32> {
        if register == 0 {
            None
        } else if let Some(host) = self.host_register(register) {
            Some(Operand32::Register(host))
        } else {
            Some(Operand32::GuestMemory(register_offset(register)))
        }
    }

    fn emit_rex(&mut self, register_field: u8, operand: Operand32, force: bool) {
        let mut rex = 0x40;
        if register_field & 8 != 0 {
            rex |= 0x04;
        }
        if matches!(operand, Operand32::Register(register) if register.encoding() & 8 != 0) {
            rex |= 0x01;
        }
        if force || rex != 0x40 {
            self.code.push(rex);
        }
    }

    fn emit_modrm(&mut self, register_field: u8, operand: Operand32) {
        match operand {
            Operand32::Register(register) => self
                .code
                .push(0xc0 | ((register_field & 7) << 3) | (register.encoding() & 7)),
            Operand32::GuestMemory(offset) => {
                self.code.push(0x40 | ((register_field & 7) << 3) | 0x06);
                self.code.push(offset);
            }
        }
    }

    fn emit_register_operand(&mut self, opcode: &[u8], destination: Register32, source: Operand32) {
        self.emit_rex(destination.encoding(), source, false);
        self.code.extend_from_slice(opcode);
        self.emit_modrm(destination.encoding(), source);
    }

    fn emit_operand_register(&mut self, opcode: &[u8], destination: Operand32, source: Register32) {
        self.emit_rex(source.encoding(), destination, false);
        self.code.extend_from_slice(opcode);
        self.emit_modrm(source.encoding(), destination);
    }

    fn emit_group_immediate(&mut self, extension: u8, destination: Operand32, value: u32) {
        self.emit_rex(extension, destination, false);
        if let Ok(value) = i8::try_from(value as i32) {
            self.code.push(0x83);
            self.emit_modrm(extension, destination);
            self.code.push(value as u8);
        } else {
            self.code.push(0x81);
            self.emit_modrm(extension, destination);
            self.code.extend_from_slice(&value.to_le_bytes());
        }
    }

    fn emit_group(&mut self, opcode: u8, extension: u8, destination: Operand32) {
        self.emit_rex(extension, destination, false);
        self.code.push(opcode);
        self.emit_modrm(extension, destination);
    }

    fn emit_shift_immediate(&mut self, extension: u8, destination: Operand32, count: u8) {
        self.emit_rex(extension, destination, false);
        self.code.push(if count == 1 { 0xd1 } else { 0xc1 });
        self.emit_modrm(extension, destination);
        if count != 1 {
            self.code.push(count);
        }
    }

    fn profile_guest_read(&mut self, register: usize) {
        #[cfg(feature = "profile")]
        if register != 0 {
            if self.cache.host(register).is_some() {
                self.increment_context(PROFILE_CACHE_READ_HITS_OFFSET);
            } else {
                self.increment_context(PROFILE_REGISTER_LOADS_OFFSET);
            }
        }
        #[cfg(not(feature = "profile"))]
        let _ = register;
    }

    fn profile_guest_write(&mut self, register: usize) {
        #[cfg(feature = "profile")]
        if register != 0 {
            if self.cache.host(register).is_some() {
                self.increment_context(PROFILE_CACHE_WRITE_HITS_OFFSET);
            } else {
                self.increment_context(PROFILE_REGISTER_STORES_OFFSET);
            }
        }
        #[cfg(not(feature = "profile"))]
        let _ = register;
    }

    fn zero_register(&mut self, register: Register32) {
        let operand = Operand32::Register(register);
        self.emit_operand_register(&[0x31], operand, register);
    }

    fn move_guest_to_register(&mut self, destination: Register32, source: usize) {
        let Some(source_operand) = self.guest_operand(source) else {
            self.zero_register(destination);
            return;
        };
        self.profile_guest_read(source);
        self.emit_register_operand(&[0x8b], destination, source_operand);
    }

    fn move_register_to_guest(&mut self, destination: usize, source: Register32) {
        if destination == 0 {
            return;
        }
        if let Some(destination_register) = self.host_register(destination) {
            if destination_register != source {
                self.emit_register_operand(
                    &[0x8b],
                    destination_register,
                    Operand32::Register(source),
                );
            }
        } else {
            self.emit_operand_register(
                &[0x89],
                Operand32::GuestMemory(register_offset(destination)),
                source,
            );
        }
        self.profile_guest_write(destination);
    }

    fn copy_guest(&mut self, destination: usize, source: usize) {
        if destination == 0 || destination == source {
            return;
        }
        if source == 0 {
            self.store_immediate(destination, 0);
            return;
        }
        if let Some(destination_register) = self.host_register(destination) {
            self.move_guest_to_register(destination_register, source);
            self.profile_guest_write(destination);
        } else if let Some(source_register) = self.host_register(source) {
            self.profile_guest_read(source);
            self.emit_operand_register(
                &[0x89],
                Operand32::GuestMemory(register_offset(destination)),
                source_register,
            );
            self.profile_guest_write(destination);
        } else {
            self.move_guest_to_register(Register32::Eax, source);
            self.move_register_to_guest(destination, Register32::Eax);
        }
    }

    fn emit_binary_register_guest(
        &mut self,
        operation: BinaryOperation32,
        destination: Register32,
        source: usize,
    ) {
        let source_operand = self
            .guest_operand(source)
            .expect("zero operands are simplified before direct binary emission");
        // Profile increments clobber flags, so account before the operation.
        self.profile_guest_read(source);
        self.emit_register_operand(operation.opcode(), destination, source_operand);
    }

    fn immediate(&mut self, destination: usize, source: usize, operation: ImmediateOperation) {
        if destination == 0 {
            return;
        }

        let constant = if source == 0 {
            Some(match operation {
                ImmediateOperation::Add(value)
                | ImmediateOperation::Xor(value)
                | ImmediateOperation::Or(value) => value,
                ImmediateOperation::And(_)
                | ImmediateOperation::ShiftLeft(_)
                | ImmediateOperation::ShiftRight(_)
                | ImmediateOperation::ShiftRightArithmetic(_) => 0,
                ImmediateOperation::SetLessThan(value) => u32::from((value as i32) > 0),
                ImmediateOperation::SetBelow(value) => u32::from(value != 0),
            })
        } else {
            match operation {
                ImmediateOperation::And(0) => Some(0),
                ImmediateOperation::Or(u32::MAX) => Some(u32::MAX),
                _ => None,
            }
        };
        if let Some(value) = constant {
            self.store_immediate(destination, value);
            return;
        }

        if matches!(
            operation,
            ImmediateOperation::Add(0)
                | ImmediateOperation::Xor(0)
                | ImmediateOperation::Or(0)
                | ImmediateOperation::And(u32::MAX)
                | ImmediateOperation::ShiftLeft(0)
                | ImmediateOperation::ShiftRight(0)
                | ImmediateOperation::ShiftRightArithmetic(0)
        ) {
            self.copy_guest(destination, source);
            return;
        }

        match operation {
            ImmediateOperation::Add(value) => self.direct_immediate(destination, source, 0, value),
            ImmediateOperation::Xor(value) => self.direct_immediate(destination, source, 6, value),
            ImmediateOperation::Or(value) => self.direct_immediate(destination, source, 1, value),
            ImmediateOperation::And(value) => self.direct_immediate(destination, source, 4, value),
            ImmediateOperation::ShiftLeft(count) => {
                self.direct_immediate_shift(destination, source, 4, count)
            }
            ImmediateOperation::ShiftRight(count) => {
                self.direct_immediate_shift(destination, source, 5, count)
            }
            ImmediateOperation::ShiftRightArithmetic(count) => {
                self.direct_immediate_shift(destination, source, 7, count)
            }
            ImmediateOperation::SetLessThan(value) => {
                self.load_eax(source);
                self.mov_ecx(value);
                self.compare_and_set(0x9c);
                self.store_eax(destination);
            }
            ImmediateOperation::SetBelow(value) => {
                self.load_eax(source);
                self.mov_ecx(value);
                self.compare_and_set(0x92);
                self.store_eax(destination);
            }
        }
    }

    fn direct_immediate(&mut self, destination: usize, source: usize, extension: u8, value: u32) {
        if destination == source {
            let destination_operand = self
                .guest_operand(destination)
                .expect("nonzero destination has an x86 operand");
            self.profile_guest_read(source);
            self.emit_group_immediate(extension, destination_operand, value);
            self.profile_guest_write(destination);
        } else if let Some(destination_register) = self.host_register(destination) {
            self.move_guest_to_register(destination_register, source);
            self.emit_group_immediate(extension, Operand32::Register(destination_register), value);
            self.profile_guest_write(destination);
        } else {
            self.move_guest_to_register(Register32::Eax, source);
            if i8::try_from(value as i32).is_ok() {
                self.emit_group_immediate(extension, Operand32::Register(Register32::Eax), value);
            } else {
                let opcode = match extension {
                    0 => 0x05,
                    1 => 0x0d,
                    4 => 0x25,
                    6 => 0x35,
                    _ => unreachable!("unsupported direct immediate group"),
                };
                self.eax_immediate(opcode, value);
            }
            self.move_register_to_guest(destination, Register32::Eax);
        }
    }

    fn direct_immediate_shift(
        &mut self,
        destination: usize,
        source: usize,
        extension: u8,
        count: u8,
    ) {
        if destination == source {
            let destination_operand = self
                .guest_operand(destination)
                .expect("nonzero destination has an x86 operand");
            self.profile_guest_read(source);
            self.emit_shift_immediate(extension, destination_operand, count);
            self.profile_guest_write(destination);
        } else if let Some(destination_register) = self.host_register(destination) {
            self.move_guest_to_register(destination_register, source);
            self.emit_shift_immediate(extension, Operand32::Register(destination_register), count);
            self.profile_guest_write(destination);
        } else {
            self.move_guest_to_register(Register32::Eax, source);
            self.emit_shift_immediate(extension, Operand32::Register(Register32::Eax), count);
            self.move_register_to_guest(destination, Register32::Eax);
        }
    }

    fn direct_register(
        &mut self,
        destination: usize,
        mut left: usize,
        mut right: usize,
        operation: BinaryOperation32,
    ) {
        if destination == 0 {
            return;
        }

        match operation {
            BinaryOperation32::Add | BinaryOperation32::Xor | BinaryOperation32::Or => {
                if left == 0 {
                    self.copy_guest(destination, right);
                    return;
                }
                if right == 0 {
                    self.copy_guest(destination, left);
                    return;
                }
            }
            BinaryOperation32::And | BinaryOperation32::Multiply => {
                if left == 0 || right == 0 {
                    self.store_immediate(destination, 0);
                    return;
                }
            }
            BinaryOperation32::Subtract => {
                if left == right {
                    self.store_immediate(destination, 0);
                    return;
                }
                if right == 0 {
                    self.copy_guest(destination, left);
                    return;
                }
                if left == 0 {
                    self.negate_guest(destination, right);
                    return;
                }
            }
        }
        if left == right {
            match operation {
                BinaryOperation32::Xor => {
                    self.store_immediate(destination, 0);
                    return;
                }
                BinaryOperation32::And | BinaryOperation32::Or => {
                    self.copy_guest(destination, left);
                    return;
                }
                _ => {}
            }
        }

        if operation.commutative() && destination == right && destination != left {
            std::mem::swap(&mut left, &mut right);
        }
        let aliases_noncommutative_right =
            !operation.commutative() && destination == right && destination != left;
        let cached_destination = self.host_register(destination);
        let result = if aliases_noncommutative_right {
            Register32::Eax
        } else {
            cached_destination.unwrap_or(Register32::Eax)
        };

        if Some(result) == cached_destination && destination == left {
            self.profile_guest_read(left);
        } else {
            self.move_guest_to_register(result, left);
        }
        self.emit_binary_register_guest(operation, result, right);
        self.move_register_to_guest(destination, result);
    }

    fn negate_guest(&mut self, destination: usize, source: usize) {
        let cached_destination = self.host_register(destination);
        let result = cached_destination.unwrap_or(Register32::Eax);
        if Some(result) == cached_destination && destination == source {
            self.profile_guest_read(source);
        } else {
            self.move_guest_to_register(result, source);
        }
        self.emit_group(0xf7, 3, Operand32::Register(result));
        self.move_register_to_guest(destination, result);
    }

    fn register(
        &mut self,
        destination: usize,
        left: usize,
        right: usize,
        operation: RegisterOperation,
    ) -> Option<()> {
        if destination == 0 {
            return Some(());
        }
        let direct = match operation {
            RegisterOperation::Add => Some(BinaryOperation32::Add),
            RegisterOperation::Subtract => Some(BinaryOperation32::Subtract),
            RegisterOperation::Xor => Some(BinaryOperation32::Xor),
            RegisterOperation::Or => Some(BinaryOperation32::Or),
            RegisterOperation::And => Some(BinaryOperation32::And),
            RegisterOperation::Multiply => Some(BinaryOperation32::Multiply),
            _ => None,
        };
        if let Some(operation) = direct {
            self.direct_register(destination, left, right, operation);
            return Some(());
        }
        self.load_eax(left);
        self.load_ecx(right);
        match operation {
            RegisterOperation::Add
            | RegisterOperation::Subtract
            | RegisterOperation::Xor
            | RegisterOperation::Or
            | RegisterOperation::And
            | RegisterOperation::Multiply => unreachable!("direct operation returned above"),
            RegisterOperation::MultiplyHighSigned => {
                self.code.extend_from_slice(&[0xf7, 0xe9]); // imul ecx
                self.mov_eax_edx();
            }
            RegisterOperation::MultiplyHighSignedUnsigned => {
                // Sign-extend the left operand, retain the right operand's
                // zero-extension from the ECX load, and form the exact mixed
                // 32x32 product in RAX without consuming the R8/R9 anchors.
                self.code.extend_from_slice(&[0x48, 0x63, 0xc0]); // movsxd rax, eax
                self.code.extend_from_slice(&[0x48, 0x0f, 0xaf, 0xc1]); // imul rax, rcx
                self.code.extend_from_slice(&[0x48, 0xc1, 0xe8, 0x20]); // shr rax, 32
            }
            RegisterOperation::MultiplyHighUnsigned => {
                self.code.extend_from_slice(&[0xf7, 0xe1]); // mul ecx
                self.mov_eax_edx();
            }
            RegisterOperation::DivideSigned => self.divide_signed()?,
            RegisterOperation::DivideUnsigned => self.divide_unsigned()?,
            RegisterOperation::RemainderSigned => self.remainder_signed()?,
            RegisterOperation::RemainderUnsigned => self.remainder_unsigned()?,
            RegisterOperation::ShiftLeft => self.code.extend_from_slice(&[0xd3, 0xe0]),
            RegisterOperation::ShiftRight => self.code.extend_from_slice(&[0xd3, 0xe8]),
            RegisterOperation::ShiftRightArithmetic => {
                self.code.extend_from_slice(&[0xd3, 0xf8]);
            }
            RegisterOperation::SetLessThan => self.compare_and_set(0x9c),
            RegisterOperation::SetBelow => self.compare_and_set(0x92),
        }
        self.store_eax(destination);
        Some(())
    }

    fn branch(
        &mut self,
        left: usize,
        right: usize,
        condition: Condition,
        fallthrough: u32,
        target: u32,
    ) -> Option<()> {
        if left == right {
            let taken = matches!(
                condition,
                Condition::Equal | Condition::GreaterOrEqual | Condition::AboveOrEqual
            );
            return self.edge_slot(if taken { target } else { fallthrough });
        }

        let condition = match (left, right, condition) {
            (0, _, Condition::Equal) | (_, 0, Condition::Equal) => {
                self.compare_guest_with_zero(left.max(right));
                Condition::Equal
            }
            (0, _, Condition::NotEqual) | (_, 0, Condition::NotEqual) => {
                self.compare_guest_with_zero(left.max(right));
                Condition::NotEqual
            }
            (_, 0, Condition::Below) => return self.edge_slot(fallthrough),
            (_, 0, Condition::AboveOrEqual) => return self.edge_slot(target),
            (0, _, Condition::Below) => {
                self.compare_guest_with_zero(right);
                Condition::NotEqual
            }
            (0, _, Condition::AboveOrEqual) => {
                self.compare_guest_with_zero(right);
                Condition::Equal
            }
            (_, 0, condition) => {
                self.compare_guest_with_zero(left);
                condition
            }
            (0, _, condition) => {
                self.zero_register(Register32::Eax);
                let right_operand = self
                    .guest_operand(right)
                    .expect("nonzero right branch operand");
                self.profile_guest_read(right);
                self.emit_register_operand(&[0x3b], Register32::Eax, right_operand);
                condition
            }
            _ => {
                let left_operand = self.guest_operand(left).expect("nonzero left operand");
                let right_operand = self.guest_operand(right).expect("nonzero right operand");
                // Account first: every profile increment changes arithmetic
                // flags and no instruction may intervene between CMP and Jcc.
                self.profile_guest_read(left);
                self.profile_guest_read(right);
                if let Operand32::Register(left_register) = left_operand {
                    self.emit_register_operand(&[0x3b], left_register, right_operand);
                } else if let Operand32::Register(right_register) = right_operand {
                    self.emit_operand_register(&[0x39], left_operand, right_register);
                } else {
                    self.emit_register_operand(&[0x8b], Register32::Eax, left_operand);
                    self.emit_register_operand(&[0x3b], Register32::Eax, right_operand);
                }
                condition
            }
        };

        #[cfg(not(feature = "profile"))]
        {
            self.conditional_edge(x86_condition(condition), target)?;
            self.edge_slot(fallthrough)
        }
        #[cfg(feature = "profile")]
        {
            self.code
                .extend_from_slice(&[0x0f, x86_condition(condition)]);
            self.code
                .extend_from_slice(&(EDGE_SLOT_BYTES as i32).to_le_bytes());
            self.edge_slot(fallthrough)?;
            self.edge_slot(target)
        }
    }

    fn compare_guest_with_zero(&mut self, register: usize) {
        let operand = self
            .guest_operand(register)
            .expect("zero comparisons require one nonzero operand");
        self.profile_guest_read(register);
        if let Operand32::Register(register) = operand {
            self.emit_operand_register(&[0x85], operand, register);
        } else {
            self.emit_group_immediate(7, operand, 0);
        }
    }

    fn indirect_jump(
        &mut self,
        pc: u32,
        destination: usize,
        source: usize,
        immediate: u32,
        link: u32,
    ) -> Option<()> {
        // Compute the target from the old source before a possibly aliasing rd
        // write. ECX retains the committed guest PC through every table check.
        self.load_eax(source);
        if immediate != 0 {
            self.add_eax_immediate(immediate);
        }
        self.code.extend_from_slice(&[0x83, 0xe0, 0xfe]); // and eax, -2
        self.code.extend_from_slice(&[0xa8, 0x02]); // test al, 2
        let misaligned = self.cold_jcc(0x85)?; // jnz precise slow path
        self.code.extend_from_slice(&[0x89, 0xc1]); // mov ecx, eax

        if destination != 0 {
            self.store_immediate(destination, link);
        }

        // JALR itself commits any aligned target. Range and fetch permission
        // failures belong to the next instruction, after the engine's limit
        // check, so every dispatch failure uses the committed missing exit.
        self.code.push(0x3d); // cmp eax, ADDRESS_SPACE_SIZE
        self.code
            .extend_from_slice(&ADDRESS_SPACE_SIZE.to_le_bytes());
        let mut misses = Vec::with_capacity(3);
        misses.push(self.cold_jcc(0x83)?); // jae missing
        self.code.extend_from_slice(&[0x89, 0xc2]); // mov edx, eax
        self.code
            .extend_from_slice(&[0xc1, 0xea, u8::try_from(PAGE_SHIFT).ok()?]);
        self.code
            .extend_from_slice(&[0x4c, 0x8b, 0x5f, DISPATCH_PAGES_OFFSET]);
        self.code.extend_from_slice(&[0x4d, 0x8b, 0x1c, 0xd3]); // mov r11, [r11+rdx*8]
        self.code.extend_from_slice(&[0x4d, 0x85, 0xdb]); // test r11, r11
        misses.push(self.cold_jcc(0x84)?); // jz missing

        let page_mask = u32::try_from(PAGE_SIZE.checked_sub(4)?).ok()?;
        self.code.push(0x25); // and eax, PAGE_SIZE - 4
        self.code.extend_from_slice(&page_mask.to_le_bytes());
        self.code.extend_from_slice(&[0xc1, 0xe8, 0x02]); // shr eax, 2
        self.code.extend_from_slice(&[0x41, 0x8b, 0x04, 0x83]); // mov eax, [r11+rax*4]
        self.code.extend_from_slice(&[0x85, 0xc0]); // test eax, eax
        misses.push(self.cold_jcc(0x84)?); // jz missing
        self.code.extend_from_slice(&[0xff, 0xc8]); // dec eax
        self.code
            .extend_from_slice(&[0x4c, 0x8b, 0x5f, CODE_BASE_OFFSET]);
        self.code.extend_from_slice(&[0x49, 0x01, 0xc3]); // add r11, rax
        #[cfg(feature = "profile")]
        self.increment_context(PROFILE_INDIRECT_HITS_OFFSET);
        self.code.extend_from_slice(&[0x41, 0xff, 0xe3]); // jmp r11

        self.indirect_misses.extend(misses);
        self.interpret_one_exit(vec![misaligned], pc, 1)
    }

    fn checked_load(
        &mut self,
        exit: PreciseExit,
        destination: usize,
        base: usize,
        immediate: u32,
        width: MemoryWidth,
        signed: bool,
    ) -> Option<()> {
        let failures = self.checked_memory_address(base, immediate, width, PERM_READ)?;
        let result = self.host_register(destination).unwrap_or(Register32::Eax);

        if destination != 0 {
            self.emit_flat_load(result, width, signed);
            if result == Register32::Eax {
                self.store_eax(destination);
            } else {
                self.profile_guest_write(destination);
            }
        }
        #[cfg(feature = "profile")]
        {
            self.increment_context(PROFILE_MEMORY_LOADS_OFFSET);
        }
        self.interpret_one_exit(failures, exit.pc, exit.refund)
    }

    fn checked_store(
        &mut self,
        exit: PreciseExit,
        base: usize,
        source: usize,
        immediate: u32,
        width: MemoryWidth,
    ) -> Option<()> {
        let failures = self.checked_memory_address(base, immediate, width, PERM_WRITE)?;
        let source = if source == 0 {
            self.zero_register(Register32::Ecx);
            Register32::Ecx
        } else if let Some(source_register) = self.host_register(source) {
            self.profile_guest_read(source);
            source_register
        } else {
            self.move_guest_to_register(Register32::Ecx, source);
            Register32::Ecx
        };
        self.emit_flat_store(source, width);
        #[cfg(feature = "profile")]
        {
            self.increment_context(PROFILE_MEMORY_STORES_OFFSET);
        }
        self.interpret_one_exit(failures, exit.pc, exit.refund)
    }

    /// Computes EAX = wrapping guest address and EDX = permission page index.
    /// Every returned fixup targets the caller's single precise slow exit.
    fn checked_memory_address(
        &mut self,
        base: usize,
        immediate: u32,
        width: MemoryWidth,
        permission: u8,
    ) -> Option<Vec<LocalFixup>> {
        self.load_eax(base);
        if immediate != 0 {
            self.add_eax_immediate(immediate);
        }

        // DirectMemory's permission table covers every RV32 page and leaves
        // pages outside the EEI at zero. Byte accesses therefore need no
        // separate bounds branch; wider accesses retain their exact alignment
        // check. Rust's precise retry decides the required trap class/order.
        let alignment_mask = width.bytes() - 1;
        let mut failures = Vec::with_capacity(2);
        if alignment_mask != 0 {
            self.code.extend_from_slice(&[0xa8, alignment_mask as u8]); // test al, mask
            failures.push(self.cold_jcc(0x85)?); // jnz slow
        }

        self.code.extend_from_slice(&[0x89, 0xc2]); // mov edx, eax
        let page_shift = u8::try_from(PAGE_SHIFT).ok()?;
        self.code.extend_from_slice(&[0xc1, 0xea, page_shift]); // shr edx, PAGE_SHIFT
        self.code
            .extend_from_slice(&[0x41, 0xf6, 0x04, 0x10, permission]); // test [r8+rdx], perm
        failures.push(self.cold_jcc(0x84)?); // jz slow
        Some(failures)
    }

    /// Emits `destination = [R9 + RAX]` after the checked-address path has
    /// established that EAX names a valid, aligned, permitted guest address.
    fn emit_flat_load(&mut self, destination: Register32, width: MemoryWidth, signed: bool) {
        // REX.B selects R9 as the SIB base; REX.R selects a high destination.
        self.code
            .push(0x41 | (u8::from(destination.encoding() & 8 != 0) << 2));
        match (width, signed) {
            (MemoryWidth::Byte, true) => self.code.extend_from_slice(&[0x0f, 0xbe]),
            (MemoryWidth::Byte, false) => self.code.extend_from_slice(&[0x0f, 0xb6]),
            (MemoryWidth::Half, true) => self.code.extend_from_slice(&[0x0f, 0xbf]),
            (MemoryWidth::Half, false) => self.code.extend_from_slice(&[0x0f, 0xb7]),
            (MemoryWidth::Word, _) => self.code.push(0x8b),
        }
        self.code.push(((destination.encoding() & 7) << 3) | 0x04);
        self.code.push(0x01); // scale 1, index RAX, base R9
    }

    /// Emits `[R9 + RAX] = source`. The mandatory REX.B prefix selects R9 and
    /// also makes byte stores from EBP use BPL rather than the legacy CH.
    fn emit_flat_store(&mut self, source: Register32, width: MemoryWidth) {
        if width == MemoryWidth::Half {
            self.code.push(0x66);
        }
        let high = source.encoding() & 8 != 0;
        self.code.push(0x41 | u8::from(high) << 2); // REX.R | REX.B
        self.code.push(if width == MemoryWidth::Byte {
            0x88
        } else {
            0x89
        });
        self.code.push(((source.encoding() & 7) << 3) | 0x04);
        self.code.push(0x01); // scale 1, index RAX, base R9
    }

    fn interpret_one_exit(&mut self, branches: Vec<LocalFixup>, pc: u32, refund: u8) -> Option<()> {
        (!branches.is_empty()).then(|| {
            self.interpret_one_exits.push(InterpretOneRelocation {
                branches,
                pc,
                refund,
            });
        })
    }

    fn divide_signed(&mut self) -> Option<()> {
        // RISC-V defines both exceptional x86 IDIV inputs. Guard them before
        // IDIV so guest arithmetic can never raise host #DE.
        self.code.extend_from_slice(&[0x85, 0xc9]); // test ecx, ecx
        let zero = self.local_jcc(0x84)?; // jz zero
        self.code.push(0x3d); // cmp eax, INT_MIN
        self.code.extend_from_slice(&i32::MIN.to_le_bytes());
        let divide = self.local_jcc(0x85)?; // jne divide
        self.code.extend_from_slice(&[0x83, 0xf9, 0xff]); // cmp ecx, -1
        let done_overflow = self.local_jcc(0x84)?; // je done, EAX already INT_MIN
        self.bind_local(divide)?;
        self.code.extend_from_slice(&[0x99, 0xf7, 0xf9]); // cdq; idiv ecx
        let done_divide = self.local_jump()?;
        self.bind_local(zero)?;
        self.mov_eax(u32::MAX);
        self.bind_local(done_overflow)?;
        self.bind_local(done_divide)
    }

    fn divide_unsigned(&mut self) -> Option<()> {
        self.code.extend_from_slice(&[0x85, 0xc9]); // test ecx, ecx
        let zero = self.local_jcc(0x84)?; // jz zero
        self.code.extend_from_slice(&[0x31, 0xd2]); // xor edx, edx
        self.code.extend_from_slice(&[0xf7, 0xf1]); // div ecx
        let done = self.local_jump()?;
        self.bind_local(zero)?;
        self.mov_eax(u32::MAX);
        self.bind_local(done)
    }

    fn remainder_signed(&mut self) -> Option<()> {
        // A zero divisor returns the dividend. INT_MIN % -1 returns zero.
        self.code.extend_from_slice(&[0x85, 0xc9]); // test ecx, ecx
        let done_zero = self.local_jcc(0x84)?; // jz done
        self.code.push(0x3d); // cmp eax, INT_MIN
        self.code.extend_from_slice(&i32::MIN.to_le_bytes());
        let divide_left = self.local_jcc(0x85)?; // jne divide
        self.code.extend_from_slice(&[0x83, 0xf9, 0xff]); // cmp ecx, -1
        let divide_right = self.local_jcc(0x85)?; // jne divide
        self.code.extend_from_slice(&[0x31, 0xc0]); // xor eax, eax
        let done_overflow = self.local_jump()?;
        self.bind_local(divide_left)?;
        self.bind_local(divide_right)?;
        self.code.extend_from_slice(&[0x99, 0xf7, 0xf9]); // cdq; idiv ecx
        self.mov_eax_edx();
        self.bind_local(done_zero)?;
        self.bind_local(done_overflow)
    }

    fn remainder_unsigned(&mut self) -> Option<()> {
        self.code.extend_from_slice(&[0x85, 0xc9]); // test ecx, ecx
        let done = self.local_jcc(0x84)?; // jz done, EAX retains dividend
        self.code.extend_from_slice(&[0x31, 0xd2]); // xor edx, edx
        self.code.extend_from_slice(&[0xf7, 0xf1]); // div ecx
        self.mov_eax_edx();
        self.bind_local(done)
    }

    fn write_immediate(&mut self, register: usize, value: u32) {
        if register != 0 {
            self.store_immediate(register, value);
        }
    }

    fn store_immediate(&mut self, register: usize, value: u32) {
        if let Some(host) = self.host_register(register) {
            if value == 0 {
                self.zero_register(host);
            } else {
                match host {
                    Register32::Ebx => self.code.push(0xbb),
                    Register32::Ebp => self.code.push(0xbd),
                    Register32::R12d => self.code.extend_from_slice(&[0x41, 0xbc]),
                    Register32::R13d => self.code.extend_from_slice(&[0x41, 0xbd]),
                    Register32::R14d => self.code.extend_from_slice(&[0x41, 0xbe]),
                    Register32::R15d => self.code.extend_from_slice(&[0x41, 0xbf]),
                    #[cfg(not(feature = "profile"))]
                    Register32::R11d => self.code.extend_from_slice(&[0x41, 0xbb]),
                    Register32::Eax | Register32::Ecx => {
                        unreachable!("scratch registers are not guest caches")
                    }
                }
                self.code.extend_from_slice(&value.to_le_bytes());
            }
            #[cfg(feature = "profile")]
            self.increment_context(PROFILE_CACHE_WRITE_HITS_OFFSET);
        } else {
            self.code
                .extend_from_slice(&[0xc7, 0x46, register_offset(register)]);
            self.code.extend_from_slice(&value.to_le_bytes());
            #[cfg(feature = "profile")]
            self.increment_context(PROFILE_REGISTER_STORES_OFFSET);
        }
    }

    fn load_eax(&mut self, register: usize) {
        if register == 0 {
            self.code.extend_from_slice(&[0x31, 0xc0]); // xor eax, eax
        } else if let Some(host) = self.host_register(register) {
            match host {
                Register32::Ebx => self.code.extend_from_slice(&[0x89, 0xd8]),
                Register32::Ebp => self.code.extend_from_slice(&[0x89, 0xe8]),
                Register32::R12d => self.code.extend_from_slice(&[0x44, 0x89, 0xe0]),
                Register32::R13d => self.code.extend_from_slice(&[0x44, 0x89, 0xe8]),
                Register32::R14d => self.code.extend_from_slice(&[0x44, 0x89, 0xf0]),
                Register32::R15d => self.code.extend_from_slice(&[0x44, 0x89, 0xf8]),
                #[cfg(not(feature = "profile"))]
                Register32::R11d => self.code.extend_from_slice(&[0x44, 0x89, 0xd8]),
                Register32::Eax | Register32::Ecx => {
                    unreachable!("scratch registers are not guest caches")
                }
            }
            #[cfg(feature = "profile")]
            self.increment_context(PROFILE_CACHE_READ_HITS_OFFSET);
        } else {
            self.code
                .extend_from_slice(&[0x8b, 0x46, register_offset(register)]);
            #[cfg(feature = "profile")]
            self.increment_context(PROFILE_REGISTER_LOADS_OFFSET);
        }
    }

    fn load_ecx(&mut self, register: usize) {
        if register == 0 {
            self.code.extend_from_slice(&[0x31, 0xc9]); // xor ecx, ecx
        } else if let Some(host) = self.host_register(register) {
            match host {
                Register32::Ebx => self.code.extend_from_slice(&[0x89, 0xd9]),
                Register32::Ebp => self.code.extend_from_slice(&[0x89, 0xe9]),
                Register32::R12d => self.code.extend_from_slice(&[0x44, 0x89, 0xe1]),
                Register32::R13d => self.code.extend_from_slice(&[0x44, 0x89, 0xe9]),
                Register32::R14d => self.code.extend_from_slice(&[0x44, 0x89, 0xf1]),
                Register32::R15d => self.code.extend_from_slice(&[0x44, 0x89, 0xf9]),
                #[cfg(not(feature = "profile"))]
                Register32::R11d => self.code.extend_from_slice(&[0x44, 0x89, 0xd9]),
                Register32::Eax | Register32::Ecx => {
                    unreachable!("scratch registers are not guest caches")
                }
            }
            #[cfg(feature = "profile")]
            self.increment_context(PROFILE_CACHE_READ_HITS_OFFSET);
        } else {
            self.code
                .extend_from_slice(&[0x8b, 0x4e, register_offset(register)]);
            #[cfg(feature = "profile")]
            self.increment_context(PROFILE_REGISTER_LOADS_OFFSET);
        }
    }

    fn store_eax(&mut self, register: usize) {
        if let Some(host) = self.host_register(register) {
            match host {
                Register32::Ebx => self.code.extend_from_slice(&[0x89, 0xc3]),
                Register32::Ebp => self.code.extend_from_slice(&[0x89, 0xc5]),
                Register32::R12d => self.code.extend_from_slice(&[0x41, 0x89, 0xc4]),
                Register32::R13d => self.code.extend_from_slice(&[0x41, 0x89, 0xc5]),
                Register32::R14d => self.code.extend_from_slice(&[0x41, 0x89, 0xc6]),
                Register32::R15d => self.code.extend_from_slice(&[0x41, 0x89, 0xc7]),
                #[cfg(not(feature = "profile"))]
                Register32::R11d => self.code.extend_from_slice(&[0x41, 0x89, 0xc3]),
                Register32::Eax | Register32::Ecx => {
                    unreachable!("scratch registers are not guest caches")
                }
            }
            #[cfg(feature = "profile")]
            self.increment_context(PROFILE_CACHE_WRITE_HITS_OFFSET);
        } else {
            self.code
                .extend_from_slice(&[0x89, 0x46, register_offset(register)]);
            #[cfg(feature = "profile")]
            self.increment_context(PROFILE_REGISTER_STORES_OFFSET);
        }
    }

    fn mov_eax(&mut self, value: u32) {
        self.code.push(0xb8);
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    fn mov_ecx(&mut self, value: u32) {
        self.code.push(0xb9);
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    fn mov_eax_edx(&mut self) {
        self.code.extend_from_slice(&[0x89, 0xd0]);
    }

    fn eax_immediate(&mut self, opcode: u8, value: u32) {
        self.code.push(opcode);
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    fn add_eax_immediate(&mut self, value: u32) {
        if i8::try_from(value as i32).is_ok() {
            self.emit_group_immediate(0, Operand32::Register(Register32::Eax), value);
        } else {
            self.eax_immediate(0x05, value);
        }
    }

    fn compare_and_set(&mut self, condition: u8) {
        self.code
            .extend_from_slice(&[0x39, 0xc8, 0x0f, condition, 0xc0, 0x0f, 0xb6, 0xc0]);
    }

    fn local_jcc(&mut self, condition: u8) -> Option<LocalFixup> {
        let fixup = self.cold_jcc(condition)?;
        self.local_fixups.push(fixup);
        Some(fixup)
    }

    fn cold_jcc(&mut self, condition: u8) -> Option<LocalFixup> {
        let displacement_offset = self.code.len().checked_add(2)?;
        let instruction_end = self.code.len().checked_add(6)?;
        self.code.extend_from_slice(&[0x0f, condition, 0, 0, 0, 0]);
        Some(LocalFixup {
            displacement_offset,
            instruction_end,
        })
    }

    fn local_jump(&mut self) -> Option<LocalFixup> {
        let displacement_offset = self.code.len().checked_add(1)?;
        let instruction_end = self.code.len().checked_add(5)?;
        self.code.extend_from_slice(&[0xe9, 0, 0, 0, 0]);
        let fixup = LocalFixup {
            displacement_offset,
            instruction_end,
        };
        self.local_fixups.push(fixup);
        Some(fixup)
    }

    fn bind_local(&mut self, fixup: LocalFixup) -> Option<()> {
        let index = self
            .local_fixups
            .iter()
            .position(|pending| *pending == fixup)?;
        let displacement =
            i64::try_from(self.code.len()).ok()? - i64::try_from(fixup.instruction_end).ok()?;
        let displacement = i32::try_from(displacement).ok()?;
        self.code
            .get_mut(fixup.displacement_offset..fixup.displacement_offset.checked_add(4)?)?
            .copy_from_slice(&displacement.to_le_bytes());
        self.local_fixups.swap_remove(index);
        Some(())
    }

    fn edge_slot(&mut self, pc: u32) -> Option<()> {
        let slot_offset = self.code.len();
        let end = slot_offset.checked_add(EDGE_SLOT_BYTES)?;
        self.code.resize(end, 0x90);
        self.edges.push(EdgeRelocation {
            slot_offset,
            target_pc: pc,
        });
        Some(())
    }

    #[cfg(not(feature = "profile"))]
    fn conditional_edge(&mut self, condition: u8, pc: u32) -> Option<()> {
        let start = self.code.len();
        self.code.extend_from_slice(&[0x0f, condition, 0, 0, 0, 0]);
        self.conditional_edges.push(ConditionalEdgeRelocation {
            branch: LocalFixup {
                displacement_offset: start.checked_add(2)?,
                instruction_end: start.checked_add(6)?,
            },
            target_pc: pc,
        });
        Some(())
    }

    fn cold_jump(&mut self) -> Option<LocalFixup> {
        let displacement_offset = self.code.len().checked_add(1)?;
        let instruction_end = self.code.len().checked_add(5)?;
        self.code.extend_from_slice(&[0xe9, 0, 0, 0, 0]);
        Some(LocalFixup {
            displacement_offset,
            instruction_end,
        })
    }

    fn emit_exit_trampolines(&mut self) -> Option<(ExitTargets, usize)> {
        let start = self.code.len();
        let interpret_one = self.code.len();
        self.store_exit_reason(EXIT_INTERPRET_ONE);
        self.code.extend_from_slice(&[0xeb, 0x10]); // jmp common tail
        let budget = self.code.len();
        self.store_exit_reason(EXIT_BUDGET);
        self.code.extend_from_slice(&[0xeb, 0x07]); // jmp common tail
        let missing = self.code.len();
        self.store_exit_reason(EXIT_MISSING);
        // Missing-successor exits dominate the normal cold paths, so let that
        // head fall through to the shared state-commit tail.
        for (host, guest) in self.cache.entries() {
            self.spill_host(host, guest);
        }
        #[cfg(feature = "profile")]
        self.add_context(PROFILE_REGISTER_STORES_OFFSET, self.cache.count())?;
        self.code
            .extend_from_slice(&[0x4c, 0x89, 0x57, REMAINING_OFFSET]);
        self.code.extend_from_slice(&[0x89, 0x47, PC_OFFSET]);
        for (host, _) in self.cache.entries().rev() {
            self.pop_host(host);
        }
        self.code.push(0xc3);
        let bytes = self.code.len().checked_sub(start)?;
        (bytes <= MAX_EXIT_TRAMPOLINE_BYTES).then_some((
            ExitTargets {
                missing,
                budget,
                interpret_one,
            },
            bytes,
        ))
    }

    fn store_exit_reason(&mut self, reason: u32) {
        self.code.extend_from_slice(&[0xc7, 0x47, EXIT_OFFSET]);
        self.code.extend_from_slice(&reason.to_le_bytes());
    }

    fn emit_budget_veneer(&mut self, relocation: BudgetRelocation, target: usize) -> Option<()> {
        let start = self.code.len();
        patch_relative(&mut self.code, relocation.branch, start)?;
        self.code
            .extend_from_slice(&[0x49, 0x83, 0xc2, relocation.count]); // add r10, count
        self.mov_eax(relocation.pc);
        let jump = self.cold_jump()?;
        patch_relative(&mut self.code, jump, target)?;
        (self.code.len().checked_sub(start)? == BUDGET_VENEER_BYTES).then_some(())
    }

    fn emit_interpret_one_veneer(
        &mut self,
        relocation: InterpretOneRelocation,
        target: usize,
    ) -> Option<()> {
        let start = self.code.len();
        for branch in relocation.branches {
            patch_relative(&mut self.code, branch, start)?;
        }
        // The failed memory instruction and the remainder of its region have
        // not committed. Refund both before Rust retries exactly that memory
        // operation. Earlier region instructions stay retired and visible.
        self.code
            .extend_from_slice(&[0x49, 0x83, 0xc2, relocation.refund]);
        self.mov_eax(relocation.pc);
        let jump = self.cold_jump()?;
        patch_relative(&mut self.code, jump, target)?;
        (self.code.len().checked_sub(start)? == INTERPRET_ONE_VENEER_BYTES).then_some(())
    }

    fn emit_indirect_missing_veneer(
        &mut self,
        branches: Vec<LocalFixup>,
        target: usize,
    ) -> Option<()> {
        if branches.is_empty() {
            return Some(());
        }
        let start = self.code.len();
        for branch in branches {
            patch_relative(&mut self.code, branch, start)?;
        }
        #[cfg(feature = "profile")]
        self.increment_context(PROFILE_INDIRECT_MISSES_OFFSET);
        self.code.extend_from_slice(&[0x89, 0xc8]); // mov eax, ecx
        let jump = self.cold_jump()?;
        patch_relative(&mut self.code, jump, target)?;
        (self.code.len().checked_sub(start)? == INDIRECT_MISSING_VENEER_BYTES).then_some(())
    }

    fn emit_missing_veneer(&mut self, pc: u32, target: usize) -> Option<usize> {
        let start = self.code.len();
        self.mov_eax(pc);
        let jump = self.cold_jump()?;
        patch_relative(&mut self.code, jump, target)?;
        (self.code.len().checked_sub(start)? == MISSING_VENEER_BYTES).then_some(start)
    }

    fn resolve(mut self) -> Option<ResolvedImage> {
        if !self.local_fixups.is_empty() {
            return None;
        }
        let mut entry_by_pc = BTreeMap::new();
        for &(pc, entry) in &self.entries {
            if entry_by_pc.insert(pc, entry).is_some() {
                return None;
            }
        }

        if self.entries.is_empty() {
            return self.code.is_empty().then_some(ResolvedImage {
                code: self.code,
                entries: Vec::new(),
                #[cfg(any(test, feature = "profile"))]
                hot_code_bytes: 0,
                #[cfg(any(test, feature = "profile"))]
                cold_code_bytes: 0,
                #[cfg(any(test, feature = "profile"))]
                external_thunk_bytes: 0,
                #[cfg(any(test, feature = "profile"))]
                shared_prologue_bytes: 0,
                #[cfg(any(test, feature = "profile"))]
                exit_trampoline_bytes: 0,
            });
        }

        let _hot_code_bytes = self.code.len();
        let (_external_thunk_bytes, _shared_prologue_bytes) =
            self.emit_external_thunks_and_prologue()?;
        let (exit_targets, _exit_trampoline_bytes) = self.emit_exit_trampolines()?;
        for relocation in std::mem::take(&mut self.budget_exits) {
            self.emit_budget_veneer(relocation, exit_targets.budget)?;
        }
        for relocation in std::mem::take(&mut self.interpret_one_exits) {
            self.emit_interpret_one_veneer(relocation, exit_targets.interpret_one)?;
        }
        let indirect_misses = std::mem::take(&mut self.indirect_misses);
        self.emit_indirect_missing_veneer(indirect_misses, exit_targets.missing)?;
        let mut missing_by_pc = BTreeMap::new();
        #[cfg(not(feature = "profile"))]
        for edge in self.conditional_edges.clone() {
            let target = if let Some(entry) = entry_by_pc.get(&edge.target_pc) {
                entry.hot_offset
            } else if let Some(&target) = missing_by_pc.get(&edge.target_pc) {
                target
            } else {
                let target = self.emit_missing_veneer(edge.target_pc, exit_targets.missing)?;
                missing_by_pc.insert(edge.target_pc, target);
                target
            };
            patch_relative(&mut self.code, edge.branch, target)?;
        }
        for edge in self.edges.clone() {
            if let Some(entry) = entry_by_pc.get(&edge.target_pc) {
                patch_edge(
                    &mut self.code,
                    edge.slot_offset,
                    entry.hot_offset,
                    entry.indirect_offset,
                )?;
            } else {
                let target = if let Some(&target) = missing_by_pc.get(&edge.target_pc) {
                    target
                } else {
                    let target = self.emit_missing_veneer(edge.target_pc, exit_targets.missing)?;
                    missing_by_pc.insert(edge.target_pc, target);
                    target
                };
                patch_edge_jump(&mut self.code, edge.slot_offset, target)?;
            }
        }
        let _cold_code_bytes = self.code.len().checked_sub(_hot_code_bytes)?;
        Some(ResolvedImage {
            code: self.code,
            entries: self.entries,
            #[cfg(any(test, feature = "profile"))]
            hot_code_bytes: _hot_code_bytes,
            #[cfg(any(test, feature = "profile"))]
            cold_code_bytes: _cold_code_bytes,
            #[cfg(any(test, feature = "profile"))]
            external_thunk_bytes: _external_thunk_bytes,
            #[cfg(any(test, feature = "profile"))]
            shared_prologue_bytes: _shared_prologue_bytes,
            #[cfg(any(test, feature = "profile"))]
            exit_trampoline_bytes: _exit_trampoline_bytes,
        })
    }
}

#[derive(Clone, Copy)]
struct ExitTargets {
    missing: usize,
    budget: usize,
    interpret_one: usize,
}

fn patch_relative(code: &mut [u8], fixup: LocalFixup, target_offset: usize) -> Option<()> {
    let displacement =
        i64::try_from(target_offset).ok()? - i64::try_from(fixup.instruction_end).ok()?;
    let displacement = i32::try_from(displacement).ok()?;
    code.get_mut(fixup.displacement_offset..fixup.displacement_offset.checked_add(4)?)?
        .copy_from_slice(&displacement.to_le_bytes());
    Some(())
}

fn patch_edge_jump(code: &mut [u8], slot_offset: usize, target_offset: usize) -> Option<()> {
    let slot = code.get_mut(slot_offset..slot_offset.checked_add(EDGE_SLOT_BYTES)?)?;
    slot.fill(0x90);
    slot[0] = 0xe9;
    let instruction_end = slot_offset.checked_add(5)?;
    let displacement = i64::try_from(target_offset).ok()? - i64::try_from(instruction_end).ok()?;
    slot[1..5].copy_from_slice(&i32::try_from(displacement).ok()?.to_le_bytes());
    Some(())
}

fn patch_edge(
    code: &mut [u8],
    slot_offset: usize,
    target_offset: usize,
    indirect_offset: usize,
) -> Option<()> {
    let slot_end = slot_offset.checked_add(EDGE_SLOT_BYTES)?;
    #[cfg(feature = "profile")]
    let jump_offset = slot_offset.checked_add(4)?;
    #[cfg(not(feature = "profile"))]
    let jump_offset = slot_offset;
    let slot = code.get_mut(slot_offset..slot_end)?;
    slot.fill(0x90);
    #[cfg(feature = "profile")]
    {
        slot[..4].copy_from_slice(&[
            0x48,
            0xff,
            0x47,
            u8::try_from(PROFILE_DIRECT_LINKS_OFFSET).ok()?,
        ]);
    }
    if indirect_offset.checked_add(ENTRY_BYTES.len())? == slot_end && target_offset == slot_end {
        let entry = indirect_offset.checked_sub(slot_offset)?;
        slot.get_mut(entry..)?.copy_from_slice(&ENTRY_BYTES);
        return Some(());
    }
    if indirect_offset == slot_end {
        return Some(());
    }
    let instruction_end = jump_offset.checked_add(5)?;
    let displacement = i64::try_from(target_offset).ok()? - i64::try_from(instruction_end).ok()?;
    let displacement = i32::try_from(displacement).ok()?;
    let jump = jump_offset - slot_offset;
    slot[jump] = 0xe9;
    slot[jump + 1..jump + 5].copy_from_slice(&displacement.to_le_bytes());
    Some(())
}

const fn register_offset(register: usize) -> u8 {
    (register * size_of::<u32>()) as u8
}
