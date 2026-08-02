//! Emits the x86-64 instruction subset shared by the native VMs.

use std::mem;

use rv32vm_rust_common::{
    machine::DecodedInstruction,
    memory::{ADDRESS_SPACE_SIZE, PAGE_SHIFT, PERM_READ, PERM_WRITE},
};

use crate::{
    BlockInstruction, SIDE_EXIT_FLAG,
    lowering::{BranchCondition, ImmediateOperation, Lowering, MemoryWidth, RegisterOperation},
};

/// Legacy/default number of basic blocks accepted by a predicted region.
///
/// Counted loops deliberately retain this bound even when a caller opts into
/// a wider finite bounded region through [`RegionLimits`].
pub const MAX_REGION_BLOCKS: usize = 4;
/// Legacy/default predicted-path instruction count accepted by one region.
pub const MAX_REGION_INSTRUCTIONS: usize = 128;
/// Largest finite bounded-region block count accepted by the generic emitter.
pub const MAX_BOUNDED_REGION_BLOCKS: usize = 16;
/// Largest finite bounded-region instruction count accepted by the emitter.
pub const MAX_BOUNDED_REGION_INSTRUCTIONS: usize = 512;
/// Counted-loop block bound, independent of wider finite regions.
pub const MAX_LOOP_BLOCKS: usize = MAX_REGION_BLOCKS;
/// Counted-loop instruction bound, independent of wider finite regions.
pub const MAX_LOOP_INSTRUCTIONS: usize = MAX_REGION_INSTRUCTIONS;

const MAX_DEFERRED_EXIT_PATCHES: usize = 1_024;
/// Largest logical-cycle group accepted by the explicit grouped-loop API.
pub const MAX_LOOP_GROUP_FACTOR: usize = 4;
/// Finalized machine-code bound for one explicitly grouped counted loop.
pub const MAX_GROUPED_LOOP_CODE_BYTES: usize = 64 * 1_024;

/// Explicit limits for finite bounded-region compilation.
///
/// Values outside the emitter hard caps, zero-sized limits, and zero code
/// budgets are rejected by the explicit bounded-region APIs. Keeping limits
/// in the call makes VM policy independent from the default shared behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionLimits {
    max_blocks: usize,
    max_instructions: usize,
    max_code_bytes: usize,
}

impl RegionLimits {
    /// Constructs an explicit finite-region policy.
    pub const fn new(max_blocks: usize, max_instructions: usize, max_code_bytes: usize) -> Self {
        Self {
            max_blocks,
            max_instructions,
            max_code_bytes,
        }
    }

    /// Legacy shared behavior used by [`CompiledBlock::compile_region`] and
    /// [`CompiledBlock::compile_unrolled_region`].
    pub const DEFAULT: Self = Self::new(MAX_REGION_BLOCKS, MAX_REGION_INSTRUCTIONS, usize::MAX);

    pub const fn max_blocks(self) -> usize {
        self.max_blocks
    }

    pub const fn max_instructions(self) -> usize {
        self.max_instructions
    }

    pub const fn max_code_bytes(self) -> usize {
        self.max_code_bytes
    }

    const fn is_valid(self) -> bool {
        self.max_blocks != 0
            && self.max_blocks <= MAX_BOUNDED_REGION_BLOCKS
            && self.max_instructions != 0
            && self.max_instructions <= MAX_BOUNDED_REGION_INSTRUCTIONS
            && self.max_code_bytes != 0
    }
}

enum Flow {
    Continue,
    Return,
}

#[derive(Clone, Copy)]
struct PreparedInstruction {
    instruction: DecodedInstruction,
    lowering: Lowering,
    preferred_successor: Option<u32>,
}

/// One decoded basic block in a bounded predicted native region.
///
/// The order of adjacent region blocks selects the preferred edge. The
/// compiler validates that edge against the preceding block's final branch,
/// direct jump, or linear fallthrough before emitting code.
#[derive(Clone, Copy)]
pub struct RegionBlock<'a> {
    instructions: &'a [BlockInstruction],
}

impl<'a> RegionBlock<'a> {
    /// Borrows one decoded block for region compilation.
    pub const fn new(instructions: &'a [BlockInstruction]) -> Self {
        Self { instructions }
    }
}

#[derive(Clone, Copy)]
enum CacheHost {
    R9,
    R10,
    R11,
    R13,
    R14,
    R15,
}

impl CacheHost {
    const CALLER_SAVED: [Self; 3] = [Self::R9, Self::R10, Self::R11];
    const CALLER_SAVED_WITH_R9_SCRATCH: [Self; 2] = [Self::R10, Self::R11];
    const LOOP_CALLEE_SAVED: [Self; 3] = [Self::R13, Self::R14, Self::R15];

    const fn code(self) -> u8 {
        match self {
            Self::R9 => 9,
            Self::R10 => 10,
            Self::R11 => 11,
            Self::R13 => 13,
            Self::R14 => 14,
            Self::R15 => 15,
        }
    }

    const fn callee_saved_bit(self) -> u8 {
        match self {
            Self::R13 => 1 << 0,
            Self::R14 => 1 << 1,
            Self::R15 => 1 << 2,
            Self::R9 | Self::R10 | Self::R11 => 0,
        }
    }
}

#[derive(Clone, Copy)]
struct PlannedRegister {
    guest: usize,
    host: CacheHost,
    written: bool,
}

#[derive(Clone, Copy, Default)]
struct RegisterScore {
    accesses: usize,
    first_access: usize,
    first_is_read: bool,
    written: bool,
}

impl RegisterScore {
    const fn bounded_savings(self) -> usize {
        self.accesses
            .saturating_sub(self.first_is_read as usize)
            .saturating_sub(self.written as usize)
    }

    const fn loop_savings(self) -> usize {
        // Counted-loop slots are unconditionally preloaded once and spilled
        // once when written. Requiring a saving over two complete cycles is a
        // conservative steady-state threshold that also admits registers used
        // only once per cycle.
        self.accesses
            .saturating_mul(2)
            .saturating_sub(1)
            .saturating_sub(self.written as usize)
    }
}

#[derive(Clone, Copy)]
enum RegisterPlanningMode {
    Bounded,
    Loop,
}

impl RegisterPlanningMode {
    const fn iterations(self) -> usize {
        match self {
            Self::Bounded => 1,
            Self::Loop => 2,
        }
    }

    const fn savings(self, score: RegisterScore) -> usize {
        match self {
            Self::Bounded => score.bounded_savings(),
            Self::Loop => score.loop_savings(),
        }
    }
}

#[derive(Clone, Copy)]
struct RegisterPlan {
    slots: [Option<PlannedRegister>; 6],
    uncached_accesses: usize,
    cached_accesses: usize,
}

impl RegisterPlan {
    fn analyze(instructions: &[PreparedInstruction]) -> Self {
        Self::analyze_with_mode(instructions, RegisterPlanningMode::Bounded)
    }

    fn analyze_loop(instructions: &[PreparedInstruction]) -> Self {
        Self::analyze_with_mode(instructions, RegisterPlanningMode::Loop)
    }

    fn analyze_with_mode(instructions: &[PreparedInstruction], mode: RegisterPlanningMode) -> Self {
        let mut scores = [RegisterScore::default(); 32];
        let mut access_index = 0;
        let mut uncached_accesses = 0_usize;

        for prepared in instructions {
            let usage = prepared.lowering.register_usage();
            for register in usage.reads.into_iter().flatten() {
                uncached_accesses += 1;
                if register != 0 {
                    let score = &mut scores[register];
                    if score.accesses == 0 {
                        score.first_access = access_index;
                        score.first_is_read = true;
                    }
                    score.accesses += 1;
                }
                access_index += 1;
            }
            if let Some(register) = usage.write.filter(|&register| register != 0) {
                uncached_accesses += 1;
                let score = &mut scores[register];
                if score.accesses == 0 {
                    score.first_access = access_index;
                }
                score.accesses += 1;
                score.written = true;
                access_index += 1;
            }
        }

        let needs_r9_scratch = instructions
            .iter()
            .any(|prepared| prepared.lowering.uses_r9_scratch());
        let caller_hosts: &[CacheHost] = if needs_r9_scratch {
            &CacheHost::CALLER_SAVED_WITH_R9_SCRATCH
        } else {
            &CacheHost::CALLER_SAVED
        };
        let callee_hosts: &[CacheHost] = if matches!(mode, RegisterPlanningMode::Loop) {
            &CacheHost::LOOP_CALLEE_SAVED
        } else {
            &[]
        };
        let mut slots = [None; 6];
        let mut selected = [false; 32];
        let mut total_savings = 0;
        let mut slot = 0;

        for (&host, minimum_savings) in caller_hosts
            .iter()
            .map(|host| (host, 0))
            .chain(callee_hosts.iter().map(|host| (host, 2)))
        {
            let best = best_register(&scores, &selected, mode, minimum_savings);
            let Some(register) = best else {
                continue;
            };
            selected[register] = true;
            total_savings += mode.savings(scores[register]);
            slots[slot] = Some(PlannedRegister {
                guest: register,
                host,
                written: scores[register].written,
            });
            slot += 1;
        }

        // Reads from x0 become constants. Allocated nonzero registers require
        // at most one initial load and one final spill on the successful path.
        let zero_reads = instructions
            .iter()
            .flat_map(|prepared| prepared.lowering.register_usage().reads)
            .flatten()
            .filter(|&register| register == 0)
            .count();
        let iterations = mode.iterations();
        let modeled_uncached_accesses = uncached_accesses.saturating_mul(iterations);
        Self {
            slots,
            uncached_accesses: modeled_uncached_accesses,
            cached_accesses: modeled_uncached_accesses
                .saturating_sub(zero_reads.saturating_mul(iterations))
                .saturating_sub(total_savings),
        }
    }

    fn saved_callee_mask(&self) -> u8 {
        self.slots
            .iter()
            .flatten()
            .fold(0, |mask, register| mask | register.host.callee_saved_bit())
    }
}

fn best_register(
    scores: &[RegisterScore; 32],
    selected: &[bool; 32],
    mode: RegisterPlanningMode,
    minimum_savings: usize,
) -> Option<usize> {
    let mut best: Option<usize> = None;
    for register in 1..32 {
        let score = scores[register];
        let savings = mode.savings(score);
        if selected[register] || savings <= minimum_savings {
            continue;
        }
        let replace = best.is_none_or(|current| {
            let current = scores[current];
            savings > mode.savings(current)
                || (savings == mode.savings(current)
                    && (score.first_access < current.first_access
                        || (score.first_access == current.first_access
                            && register < best.expect("best candidate exists"))))
        });
        if replace {
            best = Some(register);
        }
    }
    best
}

/// Execution shape of one published native entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeEntryKind {
    /// A finite block or region with a fixed maximum committed prefix.
    Bounded,
    /// A counted host loop whose instruction count describes one cycle.
    Loop,
}

/// Final machine-code bytes and their bounded-path or loop-cycle instruction count.
pub struct CompiledBlock {
    pub(crate) code: Vec<u8>,
    pub(crate) instruction_count: usize,
    pub(crate) minimum_instruction_count: usize,
    pub(crate) loop_unroll_factor: usize,
    pub(crate) kind: NativeEntryKind,
    uncached_register_accesses: usize,
    cached_register_accesses: usize,
}

impl CompiledBlock {
    /// Emits one supported native block without publishing executable memory.
    pub fn compile(instructions: &[BlockInstruction]) -> Option<Self> {
        compile(instructions)
    }

    /// Emits a bounded predicted region without publishing executable memory.
    ///
    /// A preferred branch or direct-jump edge continues into the next block.
    /// A nonpreferred branch returns normally with its actual successor and
    /// committed count; precise faults still request one-instruction fallback.
    pub fn compile_region(blocks: &[RegionBlock<'_>]) -> Option<Self> {
        compile_region(blocks)
    }

    /// Emits a finite predicted region under an explicit caller policy.
    pub fn compile_region_with_limits(
        blocks: &[RegionBlock<'_>],
        limits: RegionLimits,
    ) -> Option<Self> {
        compile_region_with_limits(blocks, limits)
    }

    /// Emits a bounded finite unroll of a predicted region.
    ///
    /// This has the same block, instruction, lowering, and adjacent-edge
    /// validation as [`Self::compile_region`], but permits repeated or
    /// overlapping guest PCs in the supplied sequence. Each occurrence is
    /// emitted separately and retains its exact committed-instruction count.
    pub fn compile_unrolled_region(blocks: &[RegionBlock<'_>]) -> Option<Self> {
        compile_unrolled_region(blocks)
    }

    /// Emits a finite repeated-PC region under an explicit caller policy.
    pub fn compile_unrolled_region_with_limits(
        blocks: &[RegionBlock<'_>],
        limits: RegionLimits,
    ) -> Option<Self> {
        compile_unrolled_region_with_limits(blocks, limits)
    }

    /// Emits one counted native loop from a bounded physical guest cycle.
    ///
    /// Every adjacent edge, including the final edge back to the first block,
    /// is validated as a preferred continuation. Blocks and guest PCs must be
    /// unique within the physical cycle; runtime repetition is supplied by a
    /// private iteration budget when the published entry executes.
    pub fn compile_loop(blocks: &[RegionBlock<'_>]) -> Option<Self> {
        compile_loop(blocks)
    }

    /// Emits an explicitly grouped counted loop.
    ///
    /// `group_factor` is the number of logical guest cycles emitted in one
    /// host-loop iteration. It must be in `1..=MAX_LOOP_GROUP_FACTOR`, and the
    /// finalized code must fit [`MAX_GROUPED_LOOP_CODE_BYTES`]. The ordinary
    /// [`Self::compile_loop`] entry point deliberately retains one-copy legacy
    /// behavior; grouping is an opt-in caller policy.
    pub fn compile_grouped_loop(blocks: &[RegionBlock<'_>], group_factor: usize) -> Option<Self> {
        compile_grouped_loop(blocks, group_factor)
    }

    pub fn code_len(&self) -> usize {
        self.code.len()
    }

    /// Maximum bounded-path count, or the instruction count of one loop cycle.
    pub const fn instruction_count(&self) -> usize {
        self.instruction_count
    }

    /// Smallest instruction budget that can enter this native code.
    ///
    /// This equals [`Self::instruction_count`] for bounded entries and
    /// one-copy counted loops. Grouped loops require the complete physical
    /// group while retaining one logical guest-cycle instruction count.
    pub const fn minimum_instruction_count(&self) -> usize {
        self.minimum_instruction_count
    }

    /// Number of logical guest loop cycles in one host-loop iteration.
    ///
    /// Bounded entries and size-capped loop fallbacks report one.
    pub const fn loop_unroll_factor(&self) -> usize {
        self.loop_unroll_factor
    }

    /// Returns whether this code is finite or a counted host loop.
    pub const fn kind(&self) -> NativeEntryKind {
        self.kind
    }

    /// Modeled uncached register-file accesses over one bounded path or the
    /// loop planner's conservative two-cycle profitability horizon.
    pub const fn uncached_register_accesses(&self) -> usize {
        self.uncached_register_accesses
    }

    /// Modeled cached accesses over one bounded path or the loop planner's
    /// conservative two-cycle profitability horizon.
    pub const fn cached_register_accesses(&self) -> usize {
        self.cached_register_accesses
    }
}

pub(super) fn compile(instructions: &[BlockInstruction]) -> Option<CompiledBlock> {
    compile_blocks(&[RegionBlock::new(instructions)], false, false, 1)
}

fn compile_region(blocks: &[RegionBlock<'_>]) -> Option<CompiledBlock> {
    compile_region_with_limits(blocks, RegionLimits::DEFAULT)
}

fn compile_unrolled_region(blocks: &[RegionBlock<'_>]) -> Option<CompiledBlock> {
    compile_unrolled_region_with_limits(blocks, RegionLimits::DEFAULT)
}

fn compile_region_with_limits(
    blocks: &[RegionBlock<'_>],
    limits: RegionLimits,
) -> Option<CompiledBlock> {
    compile_bounded_region(blocks, true, limits)
}

fn compile_unrolled_region_with_limits(
    blocks: &[RegionBlock<'_>],
    limits: RegionLimits,
) -> Option<CompiledBlock> {
    compile_bounded_region(blocks, false, limits)
}

fn compile_loop(blocks: &[RegionBlock<'_>]) -> Option<CompiledBlock> {
    compile_counted_loop(blocks, 1, usize::MAX)
}

fn compile_grouped_loop(blocks: &[RegionBlock<'_>], group_factor: usize) -> Option<CompiledBlock> {
    compile_grouped_loop_with_code_limit(blocks, group_factor, MAX_GROUPED_LOOP_CODE_BYTES)
}

fn compile_grouped_loop_with_code_limit(
    blocks: &[RegionBlock<'_>],
    group_factor: usize,
    code_limit: usize,
) -> Option<CompiledBlock> {
    if !(1..=MAX_LOOP_GROUP_FACTOR).contains(&group_factor) || code_limit == 0 {
        return None;
    }
    compile_counted_loop(blocks, group_factor, code_limit)
}

fn compile_counted_loop(
    blocks: &[RegionBlock<'_>],
    group_factor: usize,
    code_limit: usize,
) -> Option<CompiledBlock> {
    if !loop_within_bounds(blocks) {
        return None;
    }
    let compiled = compile_blocks(blocks, true, true, group_factor)?;
    (compiled.code_len() <= code_limit).then_some(compiled)
}

fn compile_bounded_region(
    blocks: &[RegionBlock<'_>],
    reject_repeated_pcs: bool,
    limits: RegionLimits,
) -> Option<CompiledBlock> {
    if !region_within_limits(blocks, limits) {
        return None;
    }
    let compiled = compile_blocks(blocks, reject_repeated_pcs && blocks.len() > 1, false, 1)?;
    (compiled.code_len() <= limits.max_code_bytes()).then_some(compiled)
}

fn region_within_limits(blocks: &[RegionBlock<'_>], limits: RegionLimits) -> bool {
    if !limits.is_valid() || blocks.is_empty() || blocks.len() > limits.max_blocks() {
        return false;
    }
    blocks
        .iter()
        .try_fold(0_usize, |count, block| {
            count.checked_add(block.instructions.len())
        })
        .is_some_and(|count| count <= limits.max_instructions())
}

fn loop_within_bounds(blocks: &[RegionBlock<'_>]) -> bool {
    if blocks.is_empty() || blocks.len() > MAX_LOOP_BLOCKS {
        return false;
    }
    blocks
        .iter()
        .try_fold(0_usize, |count, block| {
            count.checked_add(block.instructions.len())
        })
        .is_some_and(|count| count <= MAX_LOOP_INSTRUCTIONS)
}

fn compile_blocks(
    blocks: &[RegionBlock<'_>],
    reject_repeated_pcs: bool,
    closes_loop: bool,
    loop_group_factor: usize,
) -> Option<CompiledBlock> {
    let starts = blocks
        .iter()
        .map(|block| block.instructions.first()?.as_ref().ok().map(|i| i.pc()))
        .collect::<Option<Vec<_>>>()?;
    let capacity = blocks.iter().try_fold(0_usize, |count, block| {
        count.checked_add(block.instructions.len())
    })?;
    let mut prepared = Vec::with_capacity(capacity);
    let mut seen_pcs = Vec::with_capacity(capacity);
    let mut final_next_pc = *starts.first()?;
    let mut final_returned = false;

    for (block_index, block) in blocks.iter().enumerate() {
        let successor = starts
            .get(block_index + 1)
            .copied()
            .or_else(|| closes_loop.then_some(starts[0]));
        let prepared_before = prepared.len();
        let mut next_pc = starts[block_index];
        let mut consumed = 0;

        for (instruction_index, instruction) in block.instructions.iter().enumerate() {
            let Ok(instruction) = *instruction else {
                if successor.is_some() {
                    return None;
                }
                break;
            };
            if instruction.pc() != next_pc {
                if successor.is_some() {
                    return None;
                }
                break;
            }
            let Some(lowering) = Lowering::decode(instruction) else {
                if successor.is_some() {
                    return None;
                }
                break;
            };
            if reject_repeated_pcs && seen_pcs.contains(&instruction.pc()) {
                return None;
            }

            let is_last = instruction_index + 1 == block.instructions.len();
            if successor.is_some() && lowering.ends_native_block() && !is_last {
                return None;
            }
            let preferred_successor = successor.filter(|&successor| {
                is_last && valid_continuation(instruction, lowering, successor)
            });
            if successor.is_some() && is_last && preferred_successor.is_none() {
                return None;
            }

            prepared.push(PreparedInstruction {
                instruction,
                lowering,
                preferred_successor,
            });
            seen_pcs.push(instruction.pc());
            consumed += 1;
            next_pc = next_pc.wrapping_add(4);

            if lowering.ends_native_block() {
                if successor.is_none() {
                    final_returned = true;
                }
                break;
            }
        }

        if prepared.len() == prepared_before {
            return None;
        }
        if successor.is_some() && consumed != block.instructions.len() {
            return None;
        }
        if successor.is_none() {
            final_next_pc = next_pc;
        }
    }

    let instruction_count = prepared.len();
    let plan = if closes_loop {
        RegisterPlan::analyze_loop(&prepared)
    } else {
        RegisterPlan::analyze(&prepared)
    };
    let uncached_register_accesses = plan.uncached_accesses;
    let cached_register_accesses = plan.cached_accesses;

    if closes_loop {
        debug_assert!(!final_returned);
        return emit_counted_loop(&prepared, plan, starts[0], loop_group_factor);
    }

    let mut emitter = Emitter::new(plan);
    for (retired, prepared) in prepared.into_iter().enumerate() {
        let preferred_successor = prepared.preferred_successor;
        let flow = emitter.instruction(
            prepared.instruction,
            prepared.lowering,
            retired,
            preferred_successor,
        )?;
        debug_assert_eq!(
            matches!(flow, Flow::Return),
            preferred_successor.is_none() && retired + 1 == instruction_count && final_returned
        );
    }
    if !final_returned {
        emitter.return_static(final_next_pc, instruction_count, false)?;
    }
    let code = emitter.finish()?;
    Some(CompiledBlock {
        code,
        instruction_count,
        minimum_instruction_count: instruction_count,
        loop_unroll_factor: 1,
        kind: NativeEntryKind::Bounded,
        uncached_register_accesses,
        cached_register_accesses,
    })
}

fn emit_counted_loop(
    prepared: &[PreparedInstruction],
    plan: RegisterPlan,
    start: u32,
    unroll_factor: usize,
) -> Option<CompiledBlock> {
    let instruction_count = prepared.len();
    let minimum_instruction_count = instruction_count.checked_mul(unroll_factor)?;
    let uncached_register_accesses = plan.uncached_accesses;
    let cached_register_accesses = plan.cached_accesses;
    let mut emitter = Emitter::new_loop(plan, minimum_instruction_count)?;
    let loop_start = emitter.code.len();

    for copy in 0..unroll_factor {
        let retirement_base = copy.checked_mul(instruction_count)?;
        for (offset, prepared) in prepared.iter().copied().enumerate() {
            let retired = retirement_base.checked_add(offset)?;
            let flow = emitter.instruction(
                prepared.instruction,
                prepared.lowering,
                retired,
                prepared.preferred_successor,
            )?;
            if !matches!(flow, Flow::Continue) {
                return None;
            }
        }
    }

    emitter.finish_loop_group(loop_start)?;
    emitter.return_static(start, 0, false)?;
    let code = emitter.finish()?;
    Some(CompiledBlock {
        code,
        instruction_count,
        minimum_instruction_count,
        loop_unroll_factor: unroll_factor,
        kind: NativeEntryKind::Loop,
        uncached_register_accesses,
        cached_register_accesses,
    })
}

fn valid_continuation(instruction: DecodedInstruction, lowering: Lowering, successor: u32) -> bool {
    match lowering {
        Lowering::Jump { target, .. } => target.is_multiple_of(4) && successor == target,
        Lowering::Branch {
            fallthrough,
            target,
            ..
        } => successor == fallthrough || (target.is_multiple_of(4) && successor == target),
        Lowering::JumpRegister { .. } => false,
        _ => successor == instruction.pc().wrapping_add(4),
    }
}

struct DeferredExitGroup {
    pc: u32,
    retired: usize,
    dirty_mask: u8,
    needs_interpreter: bool,
    displacements: Vec<usize>,
}

#[derive(Clone, Copy)]
struct CacheSlot {
    register: PlannedRegister,
    loaded: bool,
    dirty: bool,
}

#[derive(Clone, Copy)]
enum RetirementMode {
    Bounded,
    Loop {
        retirement_quantum: usize,
        saved_callee_mask: u8,
    },
}

struct Emitter {
    code: Vec<u8>,
    deferred_exits: Vec<DeferredExitGroup>,
    deferred_exit_patches: usize,
    cache: [Option<CacheSlot>; 6],
    retirement: RetirementMode,
}

impl Emitter {
    fn new(plan: RegisterPlan) -> Self {
        Self::new_with_retirement(plan, RetirementMode::Bounded)
    }

    fn new_loop(plan: RegisterPlan, retirement_quantum: usize) -> Option<Self> {
        let saved_callee_mask = plan.saved_callee_mask();
        let mut emitter = Self::new_with_retirement(
            plan,
            RetirementMode::Loop {
                retirement_quantum,
                saved_callee_mask,
            },
        );
        // Preserve r12 for the loop counter, then only the selected callee
        // cache hosts in deterministic order. The original iteration budget
        // is pushed last so every dynamic-retirement exit reads it at [rsp].
        emitter.code.extend_from_slice(&[0x41, 0x54]); // push r12
        for host in CacheHost::LOOP_CALLEE_SAVED {
            if saved_callee_mask & host.callee_saved_bit() != 0 {
                emitter.push_host(host);
            }
        }
        emitter.code.extend_from_slice(&[
            0x51, // push rcx
            0x41, 0x89, 0xcc, // mov r12d, ecx
        ]);
        // A host backedge must not re-run lazy loads from the canonical guest
        // register array. Preload every selected slot once and conservatively
        // consider loop-written slots dirty at every static exit point. This
        // is the fixed point required for exits during later iterations.
        for slot in 0..emitter.cache.len() {
            if emitter.cache[slot].is_none() {
                continue;
            }
            emitter.ensure_cache_loaded(slot);
            if emitter.cache[slot]
                .expect("cache slot exists")
                .register
                .written
            {
                emitter.cache[slot]
                    .as_mut()
                    .expect("cache slot exists")
                    .dirty = true;
            }
        }
        Some(emitter)
    }

    fn new_with_retirement(plan: RegisterPlan, retirement: RetirementMode) -> Self {
        Self {
            // `endbr64` permits indirect entry on hosts enforcing CET. Preserve
            // the flat guest-memory base in r8 because the System V third
            // argument arrives in rdx, which RV32M division and multiplication
            // use as scratch.
            code: vec![0xf3, 0x0f, 0x1e, 0xfa, 0x49, 0x89, 0xd0],
            deferred_exits: Vec::new(),
            deferred_exit_patches: 0,
            cache: plan.slots.map(|register| {
                register.map(|register| CacheSlot {
                    register,
                    loaded: false,
                    dirty: false,
                })
            }),
            retirement,
        }
    }

    fn finish_loop_group(&mut self, loop_start: usize) -> Option<()> {
        debug_assert!(matches!(self.retirement, RetirementMode::Loop { .. }));
        self.code.extend_from_slice(&[
            0x41, 0xff, 0xcc, // dec r12d
        ]);
        let repeat = self.emit_jcc(0x85);
        self.patch_rel32(repeat, loop_start)
    }

    fn instruction(
        &mut self,
        instruction: DecodedInstruction,
        lowering: Lowering,
        retired: usize,
        preferred_successor: Option<u32>,
    ) -> Option<Flow> {
        match lowering {
            Lowering::WriteImmediate { destination, value } => {
                self.write_immediate(destination, value);
                Some(Flow::Continue)
            }
            Lowering::Jump {
                destination,
                link,
                target,
            } => self.jump(
                instruction.pc(),
                retired,
                destination,
                link,
                target,
                preferred_successor,
            ),
            Lowering::JumpRegister {
                destination,
                source,
                offset,
                link,
            } => self.jump_register(instruction.pc(), retired, destination, source, offset, link),
            Lowering::Branch {
                left,
                right,
                condition,
                fallthrough,
                target,
            } => self.branch(
                instruction.pc(),
                retired,
                left,
                right,
                condition,
                fallthrough,
                target,
                preferred_successor,
            ),
            Lowering::Immediate {
                destination,
                source,
                operation,
            } => Some(self.immediate(destination, source, operation)),
            Lowering::Register {
                destination,
                left,
                right,
                operation,
            } => self.register(
                instruction.pc(),
                retired,
                destination,
                left,
                right,
                operation,
            ),
            Lowering::Load {
                destination,
                source,
                offset,
                width,
                signed,
            } => self.load(
                instruction.pc(),
                retired,
                destination,
                source,
                offset,
                width,
                signed,
            ),
            Lowering::Store {
                source,
                base,
                offset,
                width,
            } => self.store(instruction.pc(), retired, source, base, offset, width),
            Lowering::Fence => Some(Flow::Continue),
        }
    }

    fn jump(
        &mut self,
        pc: u32,
        retired: usize,
        destination: usize,
        link: u32,
        target: u32,
        preferred_successor: Option<u32>,
    ) -> Option<Flow> {
        if !target.is_multiple_of(4) {
            self.side_exit_jump(pc, retired)?;
            return Some(Flow::Return);
        }
        self.write_immediate(destination, link);
        if preferred_successor == Some(target) {
            return Some(Flow::Continue);
        }
        self.return_static(target, retired + 1, false)?;
        Some(Flow::Return)
    }

    fn jump_register(
        &mut self,
        pc: u32,
        retired: usize,
        destination: usize,
        source: usize,
        offset: u32,
        link: u32,
    ) -> Option<Flow> {
        self.load_eax(source);
        self.eax_immediate(0x05, offset);
        self.eax_immediate(0x25, !1);
        self.code.push(0xa9);
        self.code.extend_from_slice(&3_u32.to_le_bytes());
        self.side_exit_conditional(0x85, pc, retired)?;
        if destination != 0 {
            self.mov_ecx(link);
            self.store_ecx(destination);
        }
        self.return_dynamic(retired + 1)?;
        Some(Flow::Return)
    }

    fn immediate(
        &mut self,
        destination: usize,
        source: usize,
        operation: ImmediateOperation,
    ) -> Flow {
        if destination == 0 {
            return Flow::Continue;
        }

        if destination == source
            && let Some(slot) = self.cache_index(destination)
        {
            let host = self.cache[slot].expect("cache slot exists").register.host;
            self.ensure_cache_loaded(slot);
            if self.host_immediate(host, operation) {
                self.mark_cache_written(slot);
                return Flow::Continue;
            }
        }

        self.load_eax(source);
        match operation {
            ImmediateOperation::Add(value) => self.eax_immediate(0x05, value),
            ImmediateOperation::Xor(value) => self.eax_immediate(0x35, value),
            ImmediateOperation::Or(value) => self.eax_immediate(0x0d, value),
            ImmediateOperation::And(value) => self.eax_immediate(0x25, value),
            ImmediateOperation::ShiftLeft(count) => self.eax_shift(0xe0, count),
            ImmediateOperation::ShiftRight(count) => self.eax_shift(0xe8, count),
            ImmediateOperation::ShiftRightArithmetic(count) => self.eax_shift(0xf8, count),
            ImmediateOperation::SetLessThan(value) => {
                self.mov_ecx(value);
                self.compare_and_set(0x9c);
            }
            ImmediateOperation::SetBelow(value) => {
                self.mov_ecx(value);
                self.compare_and_set(0x92);
            }
        }
        self.store_eax(destination);
        Flow::Continue
    }

    fn register(
        &mut self,
        pc: u32,
        retired: usize,
        destination: usize,
        left: usize,
        right: usize,
        operation: RegisterOperation,
    ) -> Option<Flow> {
        let exceptional_divide = matches!(
            operation,
            RegisterOperation::Divide
                | RegisterOperation::DivideUnsigned
                | RegisterOperation::Remainder
                | RegisterOperation::RemainderUnsigned
        );
        if destination == 0 && !exceptional_divide {
            return Some(Flow::Continue);
        }

        if destination != 0
            && let Some(slot) = self.cache_index(destination)
        {
            let direct = matches!(
                operation,
                RegisterOperation::Add
                    | RegisterOperation::Subtract
                    | RegisterOperation::ShiftLeft
                    | RegisterOperation::ShiftRight
                    | RegisterOperation::ShiftRightArithmetic
                    | RegisterOperation::Xor
                    | RegisterOperation::Or
                    | RegisterOperation::And
                    | RegisterOperation::Multiply
            );
            let commutative = matches!(
                operation,
                RegisterOperation::Add
                    | RegisterOperation::Xor
                    | RegisterOperation::Or
                    | RegisterOperation::And
                    | RegisterOperation::Multiply
            );
            let other = if destination == left {
                Some(right)
            } else if destination == right && commutative {
                Some(left)
            } else {
                None
            };
            if direct && let Some(other) = other {
                let host = self.cache[slot].expect("cache slot exists").register.host;
                self.ensure_cache_loaded(slot);
                self.load_ecx(other);
                if self.host_register(host, operation) {
                    self.mark_cache_written(slot);
                    return Some(Flow::Continue);
                }
            }
        }

        self.load_eax(left);
        self.load_ecx(right);
        match operation {
            RegisterOperation::Add => self.code.extend_from_slice(&[0x01, 0xc8]),
            RegisterOperation::Subtract => self.code.extend_from_slice(&[0x29, 0xc8]),
            RegisterOperation::Xor => self.code.extend_from_slice(&[0x31, 0xc8]),
            RegisterOperation::Or => self.code.extend_from_slice(&[0x09, 0xc8]),
            RegisterOperation::And => self.code.extend_from_slice(&[0x21, 0xc8]),
            RegisterOperation::Multiply => self.code.extend_from_slice(&[0x0f, 0xaf, 0xc1]),
            RegisterOperation::MultiplyHigh => {
                self.code.extend_from_slice(&[0xf7, 0xe9, 0x89, 0xd0]);
            }
            RegisterOperation::MultiplyHighUnsigned => {
                self.code.extend_from_slice(&[0xf7, 0xe1, 0x89, 0xd0]);
            }
            RegisterOperation::MultiplyHighSignedUnsigned => {
                // unsigned_high(left * right) - (left < 0 ? right : 0)
                self.code.extend_from_slice(&[
                    0x41, 0x89, 0xc1, // mov r9d, eax
                    0xf7, 0xe1, // mul ecx
                    0x41, 0xc1, 0xf9, 0x1f, // sar r9d, 31
                    0x41, 0x21, 0xc9, // and r9d, ecx
                    0x44, 0x29, 0xca, // sub edx, r9d
                    0x89, 0xd0, // mov eax, edx
                ]);
            }
            RegisterOperation::ShiftLeft => self.code.extend_from_slice(&[0xd3, 0xe0]),
            RegisterOperation::ShiftRight => self.code.extend_from_slice(&[0xd3, 0xe8]),
            RegisterOperation::ShiftRightArithmetic => {
                self.code.extend_from_slice(&[0xd3, 0xf8]);
            }
            RegisterOperation::SetLessThan => self.compare_and_set(0x9c),
            RegisterOperation::SetBelow => self.compare_and_set(0x92),
            RegisterOperation::Divide | RegisterOperation::Remainder => {
                self.signed_divide_checks(pc, retired)?;
                self.code.extend_from_slice(&[0x99, 0xf7, 0xf9]);
                if matches!(operation, RegisterOperation::Remainder) {
                    self.code.extend_from_slice(&[0x89, 0xd0]);
                }
            }
            RegisterOperation::DivideUnsigned | RegisterOperation::RemainderUnsigned => {
                self.code.extend_from_slice(&[0x85, 0xc9]);
                self.side_exit_conditional(0x84, pc, retired)?;
                self.code.extend_from_slice(&[0x31, 0xd2, 0xf7, 0xf1]);
                if matches!(operation, RegisterOperation::RemainderUnsigned) {
                    self.code.extend_from_slice(&[0x89, 0xd0]);
                }
            }
        }
        if destination != 0 {
            self.store_eax(destination);
        }
        Some(Flow::Continue)
    }

    fn signed_divide_checks(&mut self, pc: u32, retired: usize) -> Option<()> {
        self.code.extend_from_slice(&[0x85, 0xc9]);
        self.side_exit_conditional(0x84, pc, retired)?;
        self.code.push(0x3d);
        self.code.extend_from_slice(&0x8000_0000_u32.to_le_bytes());
        let not_minimum = self.emit_jcc(0x85);
        self.code.extend_from_slice(&[0x81, 0xf9]);
        self.code.extend_from_slice(&u32::MAX.to_le_bytes());
        self.side_exit_conditional(0x84, pc, retired)?;
        self.patch_rel32(not_minimum, self.code.len())
    }

    #[allow(clippy::too_many_arguments)]
    fn branch(
        &mut self,
        pc: u32,
        retired: usize,
        left: usize,
        right: usize,
        condition: BranchCondition,
        fallthrough: u32,
        target: u32,
        preferred_successor: Option<u32>,
    ) -> Option<Flow> {
        let condition = match condition {
            BranchCondition::Equal => 0x84,
            BranchCondition::NotEqual => 0x85,
            BranchCondition::LessThan => 0x8c,
            BranchCondition::GreaterOrEqual => 0x8d,
            BranchCondition::Below => 0x82,
            BranchCondition::AboveOrEqual => 0x83,
        };

        self.load_eax(left);
        self.load_ecx(right);
        self.code.extend_from_slice(&[0x39, 0xc8]);
        if !target.is_multiple_of(4) {
            self.side_exit_conditional(condition, pc, retired)?;
            if preferred_successor == Some(fallthrough) {
                return Some(Flow::Continue);
            }
            self.return_static(fallthrough, retired + 1, false)?;
            return Some(Flow::Return);
        }

        if let Some(preferred) = preferred_successor {
            if target == fallthrough {
                debug_assert_eq!(preferred, target);
                return Some(Flow::Continue);
            }
            let (mismatch_condition, mismatch_pc) = if preferred == target {
                (condition ^ 1, fallthrough)
            } else {
                debug_assert_eq!(preferred, fallthrough);
                (condition, target)
            };
            self.normal_exit_conditional(mismatch_condition, mismatch_pc, retired + 1)?;
            return Some(Flow::Continue);
        }

        let taken = self.emit_jcc(condition);
        self.return_static(fallthrough, retired + 1, false)?;
        self.patch_rel32(taken, self.code.len())?;
        self.return_static(target, retired + 1, false)?;
        Some(Flow::Return)
    }

    #[allow(clippy::too_many_arguments)]
    fn load(
        &mut self,
        pc: u32,
        retired: usize,
        destination: usize,
        source: usize,
        offset: u32,
        width: MemoryWidth,
        signed: bool,
    ) -> Option<Flow> {
        self.memory_prefix(source, offset, width, PERM_READ, pc, retired)?;
        if destination == 0 {
            return Some(Flow::Continue);
        }

        if let Some(slot) = self.cache_index(destination) {
            let host = self.cache[slot].expect("cache slot exists").register.host;
            self.load_host_memory(host, width, signed);
            self.mark_cache_written(slot);
            return Some(Flow::Continue);
        }

        match (width, signed) {
            (MemoryWidth::Byte, true) => {
                self.code.extend_from_slice(&[0x41, 0x0f, 0xbe, 0x0c, 0x00]);
            }
            (MemoryWidth::Byte, false) => {
                self.code.extend_from_slice(&[0x41, 0x0f, 0xb6, 0x0c, 0x00]);
            }
            (MemoryWidth::Half, true) => {
                self.code.extend_from_slice(&[0x41, 0x0f, 0xbf, 0x0c, 0x00]);
            }
            (MemoryWidth::Half, false) => {
                self.code.extend_from_slice(&[0x41, 0x0f, 0xb7, 0x0c, 0x00]);
            }
            (MemoryWidth::Word, _) => {
                self.code.extend_from_slice(&[0x41, 0x8b, 0x0c, 0x00]);
            }
        }
        self.code.extend_from_slice(&[0x89, 0xc8]);
        self.store_eax(destination);
        Some(Flow::Continue)
    }

    #[allow(clippy::too_many_arguments)]
    fn store(
        &mut self,
        pc: u32,
        retired: usize,
        source: usize,
        base: usize,
        offset: u32,
        width: MemoryWidth,
    ) -> Option<Flow> {
        self.memory_prefix(base, offset, width, PERM_WRITE, pc, retired)?;
        if source != 0
            && let Some(host) = self.cached_host_for_read(source)
        {
            self.store_host_memory(host, width);
            return Some(Flow::Continue);
        }
        self.load_ecx(source);
        match width {
            MemoryWidth::Byte => self.code.extend_from_slice(&[0x41, 0x88, 0x0c, 0x00]),
            MemoryWidth::Half => self.code.extend_from_slice(&[0x66, 0x41, 0x89, 0x0c, 0x00]),
            MemoryWidth::Word => self.code.extend_from_slice(&[0x41, 0x89, 0x0c, 0x00]),
        }
        Some(Flow::Continue)
    }

    fn memory_prefix(
        &mut self,
        base: usize,
        offset: u32,
        width: MemoryWidth,
        permission: u8,
        pc: u32,
        retired: usize,
    ) -> Option<()> {
        self.load_eax(base);
        self.eax_immediate(0x05, offset);
        let bytes = width.bytes();
        if bytes != 1 {
            self.code.push(0xa9);
            self.code.extend_from_slice(&(bytes - 1).to_le_bytes());
            self.side_exit_conditional(0x85, pc, retired)?;
        }

        self.code.push(0x3d);
        self.code
            .extend_from_slice(&(ADDRESS_SPACE_SIZE - bytes).to_le_bytes());
        self.side_exit_conditional(0x87, pc, retired)?;

        // Naturally aligned byte, halfword, and word accesses cannot cross a
        // 4 KiB guest page, so checking the first page is sufficient. The
        // complete guest address remains in eax for direct flat-memory access.
        self.code
            .extend_from_slice(&[0x89, 0xc1, 0xc1, 0xe9, PAGE_SHIFT as u8]);
        self.code.extend_from_slice(&[0xf6, 0x04, 0x0e, permission]);
        self.side_exit_conditional(0x84, pc, retired)?;
        Some(())
    }

    fn write_immediate(&mut self, register: usize, value: u32) {
        if register == 0 {
            return;
        }
        if let Some(slot) = self.cache_index(register) {
            let host = self.cache[slot].expect("cache slot exists").register.host;
            self.mov_host_immediate(host, value);
            self.mark_cache_written(slot);
        } else {
            self.mov_eax(value);
            self.store_register_eax(register);
        }
    }

    fn load_eax(&mut self, register: usize) {
        if register == 0 {
            self.code.extend_from_slice(&[0x31, 0xc0]);
        } else if let Some(host) = self.cached_host_for_read(register) {
            self.mov_eax_host(host);
        } else {
            self.code
                .extend_from_slice(&[0x8b, 0x47, register_offset(register)]);
        }
    }

    fn load_ecx(&mut self, register: usize) {
        if register == 0 {
            self.code.extend_from_slice(&[0x31, 0xc9]);
        } else if let Some(host) = self.cached_host_for_read(register) {
            self.mov_ecx_host(host);
        } else {
            self.code
                .extend_from_slice(&[0x8b, 0x4f, register_offset(register)]);
        }
    }

    fn store_eax(&mut self, register: usize) {
        if register == 0 {
            return;
        }
        if let Some(slot) = self.cache_index(register) {
            let host = self.cache[slot].expect("cache slot exists").register.host;
            self.mov_host_eax(host);
            self.mark_cache_written(slot);
        } else {
            self.store_register_eax(register);
        }
    }

    fn store_ecx(&mut self, register: usize) {
        if register == 0 {
            return;
        }
        if let Some(slot) = self.cache_index(register) {
            let host = self.cache[slot].expect("cache slot exists").register.host;
            self.mov_host_ecx(host);
            self.mark_cache_written(slot);
        } else {
            self.code
                .extend_from_slice(&[0x89, 0x4f, register_offset(register)]);
        }
    }

    fn store_register_eax(&mut self, register: usize) {
        self.code
            .extend_from_slice(&[0x89, 0x47, register_offset(register)]);
    }

    fn cache_index(&self, register: usize) -> Option<usize> {
        self.cache
            .iter()
            .position(|slot| slot.is_some_and(|slot| slot.register.guest == register))
    }

    fn cached_host_for_read(&mut self, register: usize) -> Option<CacheHost> {
        let slot = self.cache_index(register)?;
        self.ensure_cache_loaded(slot);
        Some(self.cache[slot].expect("cache slot exists").register.host)
    }

    fn ensure_cache_loaded(&mut self, slot: usize) {
        let cached = self.cache[slot].expect("cache slot exists");
        if cached.loaded {
            return;
        }
        let host = cached.register.host.code();
        self.code.extend_from_slice(&[
            0x44,
            0x8b,
            0x47 | ((host & 7) << 3),
            register_offset(cached.register.guest),
        ]);
        self.cache[slot].as_mut().expect("cache slot exists").loaded = true;
    }

    fn mark_cache_written(&mut self, slot: usize) {
        let cached = self.cache[slot].as_mut().expect("cache slot exists");
        cached.loaded = true;
        cached.dirty = true;
    }

    fn dirty_mask(&self) -> u8 {
        self.cache
            .iter()
            .enumerate()
            .fold(0, |mask, (slot, cached)| {
                mask | u8::from(cached.is_some_and(|cached| cached.dirty)) << slot
            })
    }

    fn emit_spills(&mut self, dirty_mask: u8) {
        for slot in 0..self.cache.len() {
            if dirty_mask & (1 << slot) == 0 {
                continue;
            }
            let cached = self.cache[slot].expect("dirty cache slot exists");
            let host = cached.register.host.code();
            self.code.extend_from_slice(&[
                0x44,
                0x89,
                0x47 | ((host & 7) << 3),
                register_offset(cached.register.guest),
            ]);
        }
    }

    fn mov_eax_host(&mut self, host: CacheHost) {
        self.code
            .extend_from_slice(&[0x44, 0x89, 0xc0 | ((host.code() & 7) << 3)]);
    }

    fn push_host(&mut self, host: CacheHost) {
        debug_assert!(host.callee_saved_bit() != 0);
        self.code
            .extend_from_slice(&[0x41, 0x50 + (host.code() & 7)]);
    }

    fn pop_host(&mut self, host: CacheHost) {
        debug_assert!(host.callee_saved_bit() != 0);
        self.code
            .extend_from_slice(&[0x41, 0x58 + (host.code() & 7)]);
    }

    fn restore_loop_stack(&mut self, saved_callee_mask: u8) {
        self.code.extend_from_slice(&[
            0x48, 0x83, 0xc4, 0x08, // add rsp, 8; discard saved rcx
        ]);
        for host in CacheHost::LOOP_CALLEE_SAVED.into_iter().rev() {
            if saved_callee_mask & host.callee_saved_bit() != 0 {
                self.pop_host(host);
            }
        }
        self.code.extend_from_slice(&[
            0x41, 0x5c, // pop r12
            0xc3, // ret
        ]);
    }

    fn mov_ecx_host(&mut self, host: CacheHost) {
        self.code
            .extend_from_slice(&[0x44, 0x89, 0xc1 | ((host.code() & 7) << 3)]);
    }

    fn mov_host_eax(&mut self, host: CacheHost) {
        self.code
            .extend_from_slice(&[0x41, 0x89, 0xc0 | (host.code() & 7)]);
    }

    fn mov_host_ecx(&mut self, host: CacheHost) {
        self.code
            .extend_from_slice(&[0x41, 0x89, 0xc8 | (host.code() & 7)]);
    }

    fn mov_host_immediate(&mut self, host: CacheHost, value: u32) {
        self.code
            .extend_from_slice(&[0x41, 0xb8 + (host.code() & 7)]);
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    fn load_host_memory(&mut self, host: CacheHost, width: MemoryWidth, signed: bool) {
        let operand = 0x04 | ((host.code() & 7) << 3);
        match (width, signed) {
            (MemoryWidth::Byte, true) => {
                self.code
                    .extend_from_slice(&[0x45, 0x0f, 0xbe, operand, 0x00]);
            }
            (MemoryWidth::Byte, false) => {
                self.code
                    .extend_from_slice(&[0x45, 0x0f, 0xb6, operand, 0x00]);
            }
            (MemoryWidth::Half, true) => {
                self.code
                    .extend_from_slice(&[0x45, 0x0f, 0xbf, operand, 0x00]);
            }
            (MemoryWidth::Half, false) => {
                self.code
                    .extend_from_slice(&[0x45, 0x0f, 0xb7, operand, 0x00]);
            }
            (MemoryWidth::Word, _) => {
                self.code.extend_from_slice(&[0x45, 0x8b, operand, 0x00]);
            }
        }
    }

    fn store_host_memory(&mut self, host: CacheHost, width: MemoryWidth) {
        let operand = 0x04 | ((host.code() & 7) << 3);
        match width {
            MemoryWidth::Byte => self.code.extend_from_slice(&[0x45, 0x88, operand, 0x00]),
            MemoryWidth::Half => {
                self.code
                    .extend_from_slice(&[0x66, 0x45, 0x89, operand, 0x00]);
            }
            MemoryWidth::Word => self.code.extend_from_slice(&[0x45, 0x89, operand, 0x00]),
        }
    }

    fn host_immediate(&mut self, host: CacheHost, operation: ImmediateOperation) -> bool {
        let extension = match operation {
            ImmediateOperation::Add(_) => 0,
            ImmediateOperation::Or(_) => 1,
            ImmediateOperation::And(_) => 4,
            ImmediateOperation::Xor(_) => 6,
            ImmediateOperation::ShiftLeft(_)
            | ImmediateOperation::ShiftRight(_)
            | ImmediateOperation::ShiftRightArithmetic(_) => {
                let (extension, count) = match operation {
                    ImmediateOperation::ShiftLeft(count) => (4, count),
                    ImmediateOperation::ShiftRight(count) => (5, count),
                    ImmediateOperation::ShiftRightArithmetic(count) => (7, count),
                    _ => unreachable!(),
                };
                self.code.extend_from_slice(&[
                    0x41,
                    0xc1,
                    0xc0 | (extension << 3) | (host.code() & 7),
                    count,
                ]);
                return true;
            }
            ImmediateOperation::SetLessThan(_) | ImmediateOperation::SetBelow(_) => return false,
        };
        let value = match operation {
            ImmediateOperation::Add(value)
            | ImmediateOperation::Xor(value)
            | ImmediateOperation::Or(value)
            | ImmediateOperation::And(value) => value,
            _ => unreachable!(),
        };
        self.code
            .extend_from_slice(&[0x41, 0x81, 0xc0 | (extension << 3) | (host.code() & 7)]);
        self.code.extend_from_slice(&value.to_le_bytes());
        true
    }

    fn host_register(&mut self, host: CacheHost, operation: RegisterOperation) -> bool {
        let opcode = match operation {
            RegisterOperation::Add => 0x01,
            RegisterOperation::Subtract => 0x29,
            RegisterOperation::Xor => 0x31,
            RegisterOperation::Or => 0x09,
            RegisterOperation::And => 0x21,
            RegisterOperation::Multiply => {
                self.code
                    .extend_from_slice(&[0x44, 0x0f, 0xaf, 0xc1 | ((host.code() & 7) << 3)]);
                return true;
            }
            RegisterOperation::ShiftLeft
            | RegisterOperation::ShiftRight
            | RegisterOperation::ShiftRightArithmetic => {
                let extension = match operation {
                    RegisterOperation::ShiftLeft => 4,
                    RegisterOperation::ShiftRight => 5,
                    RegisterOperation::ShiftRightArithmetic => 7,
                    _ => unreachable!(),
                };
                self.code.extend_from_slice(&[
                    0x41,
                    0xd3,
                    0xc0 | (extension << 3) | (host.code() & 7),
                ]);
                return true;
            }
            RegisterOperation::SetLessThan
            | RegisterOperation::SetBelow
            | RegisterOperation::MultiplyHigh
            | RegisterOperation::MultiplyHighSignedUnsigned
            | RegisterOperation::MultiplyHighUnsigned
            | RegisterOperation::Divide
            | RegisterOperation::DivideUnsigned
            | RegisterOperation::Remainder
            | RegisterOperation::RemainderUnsigned => return false,
        };
        self.code
            .extend_from_slice(&[0x41, opcode, 0xc8 | (host.code() & 7)]);
        true
    }

    fn mov_eax(&mut self, value: u32) {
        self.code.push(0xb8);
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    fn mov_ecx(&mut self, value: u32) {
        self.code.push(0xb9);
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    fn eax_immediate(&mut self, opcode: u8, value: u32) {
        self.code.push(opcode);
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    fn eax_shift(&mut self, extension: u8, count: u8) {
        self.code.extend_from_slice(&[0xc1, extension, count]);
    }

    fn compare_and_set(&mut self, condition: u8) {
        self.code
            .extend_from_slice(&[0x39, 0xc8, 0x0f, condition, 0xc0, 0x0f, 0xb6, 0xc0]);
    }

    fn return_static(&mut self, pc: u32, retired: usize, side_exit: bool) -> Option<()> {
        self.emit_spills(self.dirty_mask());
        self.return_static_unspilled(pc, retired, side_exit)
    }

    fn return_static_unspilled(&mut self, pc: u32, retired: usize, side_exit: bool) -> Option<()> {
        match self.retirement {
            RetirementMode::Bounded => {
                let outcome = encode_outcome(pc, retired, side_exit)?;
                self.code.extend_from_slice(&[0x48, 0xb8]);
                self.code.extend_from_slice(&outcome.to_le_bytes());
                self.code.push(0xc3);
                Some(())
            }
            RetirementMode::Loop {
                retirement_quantum,
                saved_callee_mask,
            } => {
                let quantum = u32::try_from(retirement_quantum).ok()?;
                let retired = u32::try_from(retired).ok()?;
                // edx = (original groups - remaining groups) * quantum
                //       + the static committed offset in the current group.
                self.code.extend_from_slice(&[
                    0x8b, 0x14, 0x24, // mov edx, dword ptr [rsp]
                    0x44, 0x29, 0xe2, // sub edx, r12d
                    0x69, 0xd2, // imul edx, edx, imm32
                ]);
                self.code.extend_from_slice(&quantum.to_le_bytes());
                if retired != 0 {
                    self.code.extend_from_slice(&[0x81, 0xc2]);
                    self.code.extend_from_slice(&retired.to_le_bytes());
                }
                if side_exit {
                    self.code.extend_from_slice(&[0x81, 0xca]);
                    self.code.extend_from_slice(&SIDE_EXIT_FLAG.to_le_bytes());
                }
                self.mov_eax(pc);
                self.code.extend_from_slice(&[
                    0x48, 0xc1, 0xe2, 0x20, // shl rdx, 32
                    0x48, 0x09, 0xd0, // or rax, rdx
                ]);
                self.restore_loop_stack(saved_callee_mask);
                Some(())
            }
        }
    }

    fn return_dynamic(&mut self, retired: usize) -> Option<()> {
        if matches!(self.retirement, RetirementMode::Loop { .. }) {
            // Loop validation rejects JALR continuations, so no counted-loop
            // exit requires both a dynamic PC and dynamic retirement count.
            return None;
        }
        self.emit_spills(self.dirty_mask());
        let high = encode_high(retired, false)?;
        self.code.push(0xba);
        self.code.extend_from_slice(&high.to_le_bytes());
        self.code
            .extend_from_slice(&[0x48, 0xc1, 0xe2, 0x20, 0x48, 0x09, 0xd0, 0xc3]);
        Some(())
    }

    fn emit_jcc(&mut self, condition: u8) -> usize {
        self.code.extend_from_slice(&[0x0f, condition]);
        let displacement = self.code.len();
        self.code.extend_from_slice(&0_i32.to_le_bytes());
        displacement
    }

    fn side_exit_conditional(&mut self, condition: u8, pc: u32, retired: usize) -> Option<()> {
        let displacement = self.emit_jcc(condition);
        self.record_deferred_exit(pc, retired, true, displacement)
    }

    fn normal_exit_conditional(&mut self, condition: u8, pc: u32, retired: usize) -> Option<()> {
        let displacement = self.emit_jcc(condition);
        self.record_deferred_exit(pc, retired, false, displacement)
    }

    fn side_exit_jump(&mut self, pc: u32, retired: usize) -> Option<()> {
        self.code.push(0xe9);
        let displacement = self.code.len();
        self.code.extend_from_slice(&0_i32.to_le_bytes());
        self.record_deferred_exit(pc, retired, true, displacement)
    }

    fn record_deferred_exit(
        &mut self,
        pc: u32,
        retired: usize,
        needs_interpreter: bool,
        displacement: usize,
    ) -> Option<()> {
        if self.deferred_exit_patches == MAX_DEFERRED_EXIT_PATCHES {
            return None;
        }
        self.deferred_exit_patches += 1;
        let dirty_mask = self.dirty_mask();
        if let Some(group) = self.deferred_exits.last_mut().filter(|group| {
            group.pc == pc
                && group.retired == retired
                && group.dirty_mask == dirty_mask
                && group.needs_interpreter == needs_interpreter
        }) {
            group.displacements.push(displacement);
        } else {
            self.deferred_exits.push(DeferredExitGroup {
                pc,
                retired,
                dirty_mask,
                needs_interpreter,
                displacements: vec![displacement],
            });
        }
        Some(())
    }

    fn patch_rel32(&mut self, displacement: usize, target: usize) -> Option<()> {
        let following = displacement.checked_add(size_of::<i32>())?;
        let delta = i64::try_from(target).ok()? - i64::try_from(following).ok()?;
        let delta = i32::try_from(delta).ok()?;
        self.code
            .get_mut(displacement..following)?
            .copy_from_slice(&delta.to_le_bytes());
        Some(())
    }

    fn finish(mut self) -> Option<Vec<u8>> {
        for group in mem::take(&mut self.deferred_exits) {
            let target = self.code.len();
            self.emit_spills(group.dirty_mask);
            self.return_static_unspilled(group.pc, group.retired, group.needs_interpreter)?;
            for displacement in group.displacements {
                self.patch_rel32(displacement, target)?;
            }
        }
        Some(self.code)
    }
}

fn encode_high(retired: usize, side_exit: bool) -> Option<u32> {
    let retired = u32::try_from(retired).ok()?;
    if retired & SIDE_EXIT_FLAG != 0 {
        return None;
    }
    Some(retired | if side_exit { SIDE_EXIT_FLAG } else { 0 })
}

fn encode_outcome(pc: u32, retired: usize, side_exit: bool) -> Option<u64> {
    Some(u64::from(pc) | (u64::from(encode_high(retired, side_exit)?) << 32))
}

const fn register_offset(register: usize) -> u8 {
    (register * size_of::<u32>()) as u8
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::machine::Machine;
    use rv32vm_rust_common::memory::IMAGE_START;

    use super::{
        MAX_DEFERRED_EXIT_PATCHES, MAX_GROUPED_LOOP_CODE_BYTES, MAX_LOOP_GROUP_FACTOR,
        MAX_REGION_BLOCKS, MAX_REGION_INSTRUCTIONS, NativeEntryKind, RegionBlock, RegionLimits,
        compile, compile_grouped_loop, compile_grouped_loop_with_code_limit, compile_loop,
        compile_region, compile_region_with_limits, compile_unrolled_region,
        compile_unrolled_region_with_limits,
    };
    use crate::{
        BlockInstruction,
        test_support::{addi, decoded_block, lw, machine_with_code},
    };

    const NOP: u32 = 0x0000_0013;

    fn branch(funct3: u32, rs1: u32, rs2: u32, offset: i32) -> u32 {
        let immediate = offset as u32 & 0x1fff;
        ((immediate >> 12) << 31)
            | (((immediate >> 5) & 0x3f) << 25)
            | (rs2 << 20)
            | (rs1 << 15)
            | (funct3 << 12)
            | (((immediate >> 1) & 0xf) << 8)
            | (((immediate >> 11) & 1) << 7)
            | 0x63
    }

    fn jal(rd: u32, offset: i32) -> u32 {
        let immediate = offset as u32 & 0x1f_ffff;
        ((immediate >> 20) << 31)
            | (((immediate >> 1) & 0x3ff) << 21)
            | (((immediate >> 11) & 1) << 20)
            | (((immediate >> 12) & 0xff) << 12)
            | (rd << 7)
            | 0x6f
    }

    fn jalr(rd: u32, rs1: u32, immediate: i32) -> u32 {
        ((immediate as u32 & 0xfff) << 20) | (rs1 << 15) | (rd << 7) | 0x67
    }

    fn register(rd: u32, rs1: u32, rs2: u32, funct3: u32, funct7: u32) -> u32 {
        (funct7 << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x33
    }

    fn decoded(machine: &Machine, start: u32, count: usize) -> Vec<BlockInstruction> {
        (0..count)
            .map(|index| machine.fetch_decode(start + index as u32 * 4))
            .collect()
    }

    #[test]
    fn compiles_supported_prefixes_only() {
        let machine = machine_with_code(
            &[addi(5, 0, 1), addi(5, 5, 1), lw(6, 0, 0), 0x0000_0073],
            IMAGE_START,
        );
        let block = decoded_block(&machine, IMAGE_START);

        let compiled = compile(&block).unwrap();

        assert_eq!(compiled.instruction_count, 3);
        assert_eq!(compiled.minimum_instruction_count(), 3);
        assert_eq!(compiled.loop_unroll_factor(), 1);
        assert_eq!(compiled.code.last(), Some(&0xc3));
    }

    #[test]
    fn compiles_single_supported_instructions() {
        let machine = machine_with_code(&[lw(6, 0, 0), 0x0000_0073], IMAGE_START);
        let block = decoded_block(&machine, IMAGE_START);

        assert_eq!(compile(&block).unwrap().instruction_count, 1);
    }

    #[test]
    fn bounded_entry_points_remain_byte_identical() {
        let machine = machine_with_code(
            &[addi(5, 5, 1), addi(5, 5, 2), branch(0, 6, 7, 8)],
            IMAGE_START,
        );
        let instructions = decoded_block(&machine, IMAGE_START);
        let basic = compile(&instructions).unwrap();
        let region = compile_region(&[RegionBlock::new(&instructions)]).unwrap();
        let unrolled = compile_unrolled_region(&[RegionBlock::new(&instructions)]).unwrap();

        assert_eq!(region.code, basic.code);
        assert_eq!(unrolled.code, basic.code);
        for compiled in [basic, region, unrolled] {
            assert_eq!(compiled.instruction_count(), 3);
            assert_eq!(compiled.minimum_instruction_count(), 3);
            assert_eq!(compiled.loop_unroll_factor(), 1);
            assert_eq!(compiled.kind(), NativeEntryKind::Bounded);
        }
    }

    #[test]
    fn rejects_unsupported_encodings() {
        let invalid = [
            (1 << 25) | (1 << 12) | (5 << 7) | 0x13,
            (2 << 25) | (7 << 20) | (6 << 15) | (5 << 7) | 0x33,
            (1 << 12) | 0x0f,
            (2 << 12) | 0x63,
        ];

        for instruction in invalid {
            let machine = machine_with_code(&[instruction, addi(0, 0, 0)], IMAGE_START);
            let block = decoded_block(&machine, IMAGE_START);
            assert!(compile(&block).is_none());
        }
    }

    #[test]
    fn caches_repeated_register_updates_and_folds_x0() {
        let machine = machine_with_code(
            &[
                addi(5, 5, 1),
                addi(5, 5, 2),
                addi(5, 5, 3),
                addi(5, 5, 4),
                0x0000_0073,
            ],
            IMAGE_START,
        );
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();

        assert_eq!(compiled.uncached_register_accesses(), 8);
        assert_eq!(compiled.cached_register_accesses(), 2);

        let machine = machine_with_code(&[addi(5, 0, 1), addi(6, 0, 2), 0x0000_0073], IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert_eq!(compiled.uncached_register_accesses(), 4);
        assert_eq!(compiled.cached_register_accesses(), 2);
    }

    #[test]
    fn loop_planner_selects_registers_profitable_within_two_cycles() {
        // x5 is read only once per cycle. A bounded plan would not preload it,
        // but one loop preload replaces two recurring canonical reads.
        let machine = machine_with_code(&[branch(1, 5, 0, 0)], IMAGE_START);
        let read_once = decoded(&machine, IMAGE_START, 1);
        let compiled = compile_loop(&[RegionBlock::new(&read_once)]).unwrap();
        assert_eq!(compiled.uncached_register_accesses(), 4);
        assert_eq!(compiled.cached_register_accesses(), 1);

        // x5 has one read and one write per cycle. Over two cycles, its one
        // unconditional preload and final spill replace four array accesses.
        let machine = machine_with_code(&[addi(5, 5, 1), jal(0, -4)], IMAGE_START);
        let read_write = decoded(&machine, IMAGE_START, 2);
        let bounded = compile(&read_write).unwrap();
        assert_eq!(bounded.uncached_register_accesses(), 2);
        assert_eq!(bounded.cached_register_accesses(), 2);

        let compiled = compile_loop(&[RegionBlock::new(&read_write)]).unwrap();
        assert_eq!(compiled.uncached_register_accesses(), 4);
        assert_eq!(compiled.cached_register_accesses(), 2);
    }

    #[test]
    fn loop_planner_uses_profitable_callee_slots_without_widening_bounded_plans() {
        let mut code = Vec::new();
        for register in 5..=10 {
            code.push(addi(register, register, 1));
            code.push(addi(register, register, 1));
        }
        code.push(jal(0, -48));
        let machine = machine_with_code(&code, IMAGE_START);
        let instructions = decoded(&machine, IMAGE_START, code.len());

        let bounded = compile(&instructions).unwrap();
        assert_eq!(bounded.uncached_register_accesses(), 24);
        assert_eq!(bounded.cached_register_accesses(), 18);

        let counted = compile_loop(&[RegionBlock::new(&instructions)]).unwrap();
        assert_eq!(counted.uncached_register_accesses(), 48);
        assert_eq!(counted.cached_register_accesses(), 12);
    }

    #[test]
    fn loop_callee_slots_require_savings_greater_than_save_restore_cost() {
        let mut break_even = Vec::new();
        for register in 5..=7 {
            break_even.push(addi(register, register, 1));
            break_even.push(addi(register, register, 1));
        }
        break_even.push(addi(8, 8, 1));
        break_even.push(jal(0, -28));
        let machine = machine_with_code(&break_even, IMAGE_START);
        let instructions = decoded(&machine, IMAGE_START, break_even.len());
        let compiled = compile_loop(&[RegionBlock::new(&instructions)]).unwrap();
        assert_eq!(compiled.uncached_register_accesses(), 28);
        assert_eq!(compiled.cached_register_accesses(), 10);

        let mut profitable = Vec::new();
        for register in 5..=7 {
            profitable.push(addi(register, register, 1));
            profitable.push(addi(register, register, 1));
        }
        profitable.push(lw(0, 8, 0));
        profitable.push(lw(0, 8, 0));
        profitable.push(jal(0, -32));
        let machine = machine_with_code(&profitable, IMAGE_START);
        let instructions = decoded(&machine, IMAGE_START, profitable.len());
        let compiled = compile_loop(&[RegionBlock::new(&instructions)]).unwrap();
        assert_eq!(compiled.uncached_register_accesses(), 28);
        assert_eq!(compiled.cached_register_accesses(), 7);
    }

    #[test]
    fn loop_r9_scratch_plan_retains_five_cache_hosts() {
        let mut code = Vec::new();
        for register in 5..=10 {
            code.push(addi(register, register, 1));
            code.push(addi(register, register, 1));
        }
        code.push(register(20, 21, 22, 2, 1)); // mulhsu uses r9d as scratch
        code.push(jal(0, -52));
        let machine = machine_with_code(&code, IMAGE_START);
        let instructions = decoded(&machine, IMAGE_START, code.len());
        let compiled = compile_loop(&[RegionBlock::new(&instructions)]).unwrap();

        assert_eq!(compiled.uncached_register_accesses(), 54);
        assert_eq!(compiled.cached_register_accesses(), 24);
    }

    #[test]
    fn compiles_valid_regions_with_one_register_plan() {
        let machine = machine_with_code(
            &[addi(5, 5, 1), branch(0, 6, 7, 12), NOP, NOP, addi(5, 5, 4)],
            IMAGE_START,
        );
        let root = decoded(&machine, IMAGE_START, 2);
        let preferred = decoded(&machine, IMAGE_START + 16, 1);

        let compiled =
            compile_region(&[RegionBlock::new(&root), RegionBlock::new(&preferred)]).unwrap();

        assert_eq!(compiled.instruction_count(), 3);
        assert_eq!(compiled.minimum_instruction_count(), 3);
        assert_eq!(compiled.loop_unroll_factor(), 1);
        assert_eq!(compiled.uncached_register_accesses(), 6);
        assert_eq!(compiled.cached_register_accesses(), 4);
    }

    #[test]
    fn validates_region_control_flow_and_rejects_repeated_pcs() {
        let machine = machine_with_code(
            &[branch(0, 6, 7, 16), jal(1, 12), jalr(1, 6, 0), NOP, NOP],
            IMAGE_START,
        );
        let branch_root = decoded(&machine, IMAGE_START, 1);
        let wrong_branch_successor = decoded(&machine, IMAGE_START + 12, 1);
        assert!(
            compile_region(&[
                RegionBlock::new(&branch_root),
                RegionBlock::new(&wrong_branch_successor),
            ])
            .is_none()
        );
        assert!(
            compile_unrolled_region(&[
                RegionBlock::new(&branch_root),
                RegionBlock::new(&wrong_branch_successor),
            ])
            .is_none()
        );

        let branch_before_end = decoded(&machine, IMAGE_START, 2);
        let branch_target = decoded(&machine, IMAGE_START + 16, 1);
        assert!(
            compile_unrolled_region(&[
                RegionBlock::new(&branch_before_end),
                RegionBlock::new(&branch_target),
            ])
            .is_none()
        );

        let jump_root = decoded(&machine, IMAGE_START + 4, 1);
        let wrong_jump_successor = decoded(&machine, IMAGE_START + 8, 1);
        assert!(
            compile_region(&[
                RegionBlock::new(&jump_root),
                RegionBlock::new(&wrong_jump_successor),
            ])
            .is_none()
        );

        let jalr_root = decoded(&machine, IMAGE_START + 8, 1);
        let jalr_fallthrough = decoded(&machine, IMAGE_START + 12, 1);
        assert!(
            compile_region(&[
                RegionBlock::new(&jalr_root),
                RegionBlock::new(&jalr_fallthrough),
            ])
            .is_none()
        );

        let syscall_machine = machine_with_code(&[0x0000_0073, NOP], IMAGE_START);
        let syscall = decoded(&syscall_machine, IMAGE_START, 1);
        let syscall_fallthrough = decoded(&syscall_machine, IMAGE_START + 4, 1);
        assert!(
            compile_region(&[
                RegionBlock::new(&syscall),
                RegionBlock::new(&syscall_fallthrough),
            ])
            .is_none()
        );

        let unsupported_successor_machine =
            machine_with_code(&[branch(0, 0, 0, 8), NOP, 0x0000_0073], IMAGE_START);
        let root = decoded(&unsupported_successor_machine, IMAGE_START, 1);
        let unsupported = decoded(&unsupported_successor_machine, IMAGE_START + 8, 1);
        assert!(
            compile_region(&[RegionBlock::new(&root), RegionBlock::new(&unsupported),]).is_none()
        );

        let repeat_machine = machine_with_code(&[branch(0, 0, 0, 0)], IMAGE_START);
        let repeated = decoded(&repeat_machine, IMAGE_START, 1);
        assert!(
            compile_region(&[RegionBlock::new(&repeated), RegionBlock::new(&repeated),]).is_none()
        );
        let unrolled =
            compile_unrolled_region(&[RegionBlock::new(&repeated), RegionBlock::new(&repeated)])
                .unwrap();
        assert_eq!(unrolled.instruction_count(), 2);
        assert_eq!(unrolled.minimum_instruction_count(), 2);
        assert_eq!(unrolled.loop_unroll_factor(), 1);

        let overlap_machine = machine_with_code(&[NOP, branch(0, 0, 0, 0)], IMAGE_START);
        let overlap_root = decoded(&overlap_machine, IMAGE_START, 2);
        let overlapping_successor = decoded(&overlap_machine, IMAGE_START + 4, 1);
        assert!(
            compile_region(&[
                RegionBlock::new(&overlap_root),
                RegionBlock::new(&overlapping_successor),
            ])
            .is_none()
        );
        assert_eq!(
            compile_unrolled_region(&[
                RegionBlock::new(&overlap_root),
                RegionBlock::new(&overlapping_successor),
            ])
            .unwrap()
            .instruction_count(),
            3
        );
    }

    #[test]
    fn validates_head_closing_counted_loops() {
        let machine = machine_with_code(
            &[addi(5, 5, 1), branch(1, 5, 6, -4), jalr(0, 1, 0)],
            IMAGE_START,
        );
        let self_loop = decoded(&machine, IMAGE_START, 2);
        let compiled = compile_loop(&[RegionBlock::new(&self_loop)]).unwrap();
        assert_eq!(compiled.kind(), NativeEntryKind::Loop);
        assert_eq!(compiled.instruction_count(), 2);
        assert_eq!(compiled.minimum_instruction_count(), 2);
        assert_eq!(compiled.loop_unroll_factor(), 1);

        let grouped = compile_grouped_loop(&[RegionBlock::new(&self_loop)], 4).unwrap();
        assert_eq!(grouped.kind(), NativeEntryKind::Loop);
        assert_eq!(grouped.instruction_count(), 2);
        assert_eq!(grouped.minimum_instruction_count(), 8);
        assert_eq!(grouped.loop_unroll_factor(), 4);
        assert!(compile_grouped_loop(&[RegionBlock::new(&self_loop)], 0).is_none());
        assert!(
            compile_grouped_loop(&[RegionBlock::new(&self_loop)], MAX_LOOP_GROUP_FACTOR + 1,)
                .is_none()
        );

        let machine = machine_with_code(&[NOP, jal(0, -4)], IMAGE_START);
        let first = decoded(&machine, IMAGE_START, 1);
        let second = decoded(&machine, IMAGE_START + 4, 1);
        let compiled =
            compile_loop(&[RegionBlock::new(&first), RegionBlock::new(&second)]).unwrap();
        assert_eq!(compiled.kind(), NativeEntryKind::Loop);
        assert_eq!(compiled.instruction_count(), 2);

        let nonclosing = decoded(&machine, IMAGE_START, 1);
        assert!(compile_loop(&[RegionBlock::new(&nonclosing)]).is_none());

        let machine = machine_with_code(&[jalr(0, 1, 0)], IMAGE_START);
        let dynamic = decoded(&machine, IMAGE_START, 1);
        assert!(compile_loop(&[RegionBlock::new(&dynamic)]).is_none());

        assert!(
            compile_loop(&[RegionBlock::new(&self_loop), RegionBlock::new(&self_loop),]).is_none()
        );
        assert!(compile_loop(&[]).is_none());
    }

    #[test]
    fn explicit_grouped_loop_enforces_its_finalized_code_limit() {
        let mut code = vec![register(5, 6, 7, 4, 1); MAX_REGION_INSTRUCTIONS - 1];
        code.push(jal(0, -508));
        let machine = machine_with_code(&code, IMAGE_START);
        let instructions = decoded(&machine, IMAGE_START, code.len());
        let blocks = [RegionBlock::new(&instructions)];

        let uncapped = compile_grouped_loop_with_code_limit(&blocks, 4, usize::MAX).unwrap();
        assert_eq!(uncapped.instruction_count(), MAX_REGION_INSTRUCTIONS);
        assert_eq!(
            uncapped.minimum_instruction_count(),
            4 * MAX_REGION_INSTRUCTIONS
        );
        assert_eq!(uncapped.loop_unroll_factor(), 4);
        assert_eq!(MAX_GROUPED_LOOP_CODE_BYTES, 64 * 1_024);

        let default = compile_loop(&blocks).unwrap();
        assert_eq!(default.minimum_instruction_count(), MAX_REGION_INSTRUCTIONS);
        assert_eq!(default.loop_unroll_factor(), 1);

        let explicit = compile_grouped_loop(&blocks, 4).unwrap();
        assert_eq!(explicit.loop_unroll_factor(), 4);
        assert!(explicit.code_len() <= MAX_GROUPED_LOOP_CODE_BYTES);

        let exact = compile_grouped_loop_with_code_limit(&blocks, 4, uncapped.code_len()).unwrap();
        assert_eq!(exact.loop_unroll_factor(), 4);

        assert!(
            compile_grouped_loop_with_code_limit(&blocks, 4, uncapped.code_len() - 1).is_none()
        );
    }

    #[test]
    fn explicit_grouped_loop_encoder_limit_does_not_change_default_loop_policy() {
        let load_count = MAX_REGION_INSTRUCTIONS - 1;
        let mut code = vec![lw(5, 6, 0); load_count];
        code.push(jal(0, -508));
        let machine = machine_with_code(&code, IMAGE_START);
        let instructions = decoded(&machine, IMAGE_START, code.len());

        // Each checked word load records alignment, range, and permission
        // exits. One copy remains bounded while four copies exceed the
        // encoder's deferred-patch budget.
        assert!(load_count * 3 <= MAX_DEFERRED_EXIT_PATCHES);
        assert!(load_count * MAX_LOOP_GROUP_FACTOR * 3 > MAX_DEFERRED_EXIT_PATCHES);

        let compiled = compile_loop(&[RegionBlock::new(&instructions)]).unwrap();
        assert_eq!(compiled.instruction_count(), MAX_REGION_INSTRUCTIONS);
        assert_eq!(
            compiled.minimum_instruction_count(),
            MAX_REGION_INSTRUCTIONS
        );
        assert_eq!(compiled.loop_unroll_factor(), 1);
        assert!(compile_grouped_loop(&[RegionBlock::new(&instructions)], 4).is_none());
    }

    #[test]
    fn enforces_region_bounds_without_narrowing_single_block_compile() {
        let machine = machine_with_code(&vec![NOP; MAX_REGION_INSTRUCTIONS + 1], IMAGE_START);
        let oversized = decoded(&machine, IMAGE_START, MAX_REGION_INSTRUCTIONS + 1);
        assert_eq!(
            compile(&oversized).unwrap().instruction_count(),
            MAX_REGION_INSTRUCTIONS + 1
        );

        let first = decoded(&machine, IMAGE_START, 64);
        let second = decoded(
            &machine,
            IMAGE_START + 64 * 4,
            MAX_REGION_INSTRUCTIONS + 1 - 64,
        );
        assert!(compile_region(&[RegionBlock::new(&first), RegionBlock::new(&second),]).is_none());
        assert!(
            compile_unrolled_region(&[RegionBlock::new(&first), RegionBlock::new(&second),])
                .is_none()
        );
        assert!(compile_loop(&[RegionBlock::new(&first), RegionBlock::new(&second)]).is_none());

        let short_machine = machine_with_code(&[NOP; MAX_REGION_BLOCKS + 1], IMAGE_START);
        let owned = (0..MAX_REGION_BLOCKS + 1)
            .map(|index| decoded(&short_machine, IMAGE_START + index as u32 * 4, 1))
            .collect::<Vec<_>>();
        let blocks = owned
            .iter()
            .map(|instructions| RegionBlock::new(instructions))
            .collect::<Vec<_>>();
        assert!(compile_region(&blocks).is_none());
        assert!(compile_unrolled_region(&blocks).is_none());
        assert!(compile_loop(&blocks).is_none());
        assert!(compile_region(&[]).is_none());
        assert!(compile_unrolled_region(&[]).is_none());
        assert!(compile_loop(&[]).is_none());
    }

    #[test]
    fn explicit_bounded_limits_widen_regions_without_widening_loops_or_defaults() {
        let code = [NOP, NOP, NOP, NOP, jal(0, -16)];
        let machine = machine_with_code(&code, IMAGE_START);
        let owned = (0..code.len())
            .map(|index| decoded(&machine, IMAGE_START + index as u32 * 4, 1))
            .collect::<Vec<_>>();
        let blocks = owned
            .iter()
            .map(|instructions| RegionBlock::new(instructions))
            .collect::<Vec<_>>();
        let limits = RegionLimits::new(8, 256, usize::MAX);

        assert_eq!(
            compile_region_with_limits(&blocks, limits)
                .unwrap()
                .instruction_count(),
            code.len()
        );
        assert!(compile_region(&blocks).is_none());
        assert!(compile_loop(&blocks).is_none());

        let long_machine = machine_with_code(&[NOP; 256], IMAGE_START);
        let long_owned = (0..4)
            .map(|index| decoded(&long_machine, IMAGE_START + index * 64 * 4, 64))
            .collect::<Vec<_>>();
        let long_blocks = long_owned
            .iter()
            .map(|instructions| RegionBlock::new(instructions))
            .collect::<Vec<_>>();
        assert_eq!(
            compile_region_with_limits(&long_blocks, limits)
                .unwrap()
                .instruction_count(),
            256
        );
        assert!(compile_region(&long_blocks).is_none());
        assert!(compile_loop(&long_blocks).is_none());
    }

    #[test]
    fn explicit_bounded_limits_enforce_hard_and_finalized_code_bounds() {
        let machine = machine_with_code(&[NOP, NOP], IMAGE_START);
        let first = decoded(&machine, IMAGE_START, 1);
        let second = decoded(&machine, IMAGE_START + 4, 1);
        let blocks = [RegionBlock::new(&first), RegionBlock::new(&second)];
        let uncapped =
            compile_region_with_limits(&blocks, RegionLimits::new(8, 256, usize::MAX)).unwrap();

        assert!(
            compile_region_with_limits(&blocks, RegionLimits::new(8, 256, uncapped.code_len()),)
                .is_some()
        );
        assert!(
            compile_region_with_limits(
                &blocks,
                RegionLimits::new(8, 256, uncapped.code_len() - 1),
            )
            .is_none()
        );
        assert!(
            compile_region_with_limits(&blocks, RegionLimits::new(0, 256, usize::MAX)).is_none()
        );
        assert!(
            compile_region_with_limits(&blocks, RegionLimits::new(17, 256, usize::MAX)).is_none()
        );
        assert!(
            compile_region_with_limits(&blocks, RegionLimits::new(8, 513, usize::MAX)).is_none()
        );
        assert!(compile_region_with_limits(&blocks, RegionLimits::new(8, 256, 0)).is_none());
    }

    #[test]
    fn explicit_unrolled_limits_accept_eight_repeated_blocks_only_when_opted_in() {
        let machine = machine_with_code(&[branch(0, 0, 0, 0)], IMAGE_START);
        let repeated = decoded(&machine, IMAGE_START, 1);
        let blocks = vec![RegionBlock::new(&repeated); 8];
        let limits = RegionLimits::new(8, 256, usize::MAX);

        assert_eq!(
            compile_unrolled_region_with_limits(&blocks, limits)
                .unwrap()
                .instruction_count(),
            8
        );
        assert!(compile_unrolled_region(&blocks).is_none());
        assert!(compile_region_with_limits(&blocks, limits).is_none());
    }
}
