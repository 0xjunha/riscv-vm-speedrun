//! Emits the x86-64 instruction subset shared by the native VMs.

use std::mem;

use rv32vm_rust_common::{
    machine::DecodedInstruction,
    memory::{PAGE_SHIFT, PERM_READ, PERM_WRITE},
};

use crate::{
    BlockInstruction, SIDE_EXIT_FLAG,
    lowering::{
        BranchCondition, ImmediateOperation, Lowering, MemoryWidth, RegisterOperation,
        RegisterUsage,
    },
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
    memory_validation: MemoryValidation,
}

#[derive(Clone, Copy)]
struct MemoryValidation {
    alignment: bool,
    permission: bool,
}

impl MemoryValidation {
    const FULL: Self = Self {
        alignment: true,
        permission: true,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SymbolicValue {
    Constant(u32),
    Affine { root: u16, offset: u32 },
}

impl SymbolicValue {
    const fn wrapping_add(self, value: u32) -> Self {
        match self {
            Self::Constant(current) => Self::Constant(current.wrapping_add(value)),
            Self::Affine { root, offset } => Self::Affine {
                root,
                offset: offset.wrapping_add(value),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProvenPage {
    Constant(u32),
    ExactAddress(SymbolicValue),
}

impl ProvenPage {
    const fn for_address(address: SymbolicValue) -> Self {
        match address {
            SymbolicValue::Constant(address) => Self::Constant(address >> PAGE_SHIFT),
            address => Self::ExactAddress(address),
        }
    }
}

#[derive(Clone, Copy)]
struct PermissionProof {
    page: ProvenPage,
    permission: u8,
}

#[derive(Clone, Copy)]
struct AlignmentProof {
    address: SymbolicValue,
    bytes: u32,
}

#[derive(Clone, Copy, Default)]
struct MemoryCheckStats {
    accesses: usize,
    alignment_candidates: usize,
    alignment_checks: usize,
    permission_checks: usize,
}

impl MemoryCheckStats {
    fn for_instructions(instructions: &[PreparedInstruction]) -> Self {
        instructions
            .iter()
            .fold(Self::default(), |mut stats, prepared| {
                let width = match prepared.lowering {
                    Lowering::Load { width, .. } | Lowering::Store { width, .. } => width,
                    _ => return stats,
                };
                stats.accesses += 1;
                let needs_alignment = width.bytes() != 1;
                stats.alignment_candidates += usize::from(needs_alignment);
                stats.alignment_checks +=
                    usize::from(needs_alignment && prepared.memory_validation.alignment);
                stats.permission_checks += usize::from(prepared.memory_validation.permission);
                stats
            })
    }

    fn repeated(self, copies: usize) -> Option<Self> {
        Some(Self {
            accesses: self.accesses.checked_mul(copies)?,
            alignment_candidates: self.alignment_candidates.checked_mul(copies)?,
            alignment_checks: self.alignment_checks.checked_mul(copies)?,
            permission_checks: self.permission_checks.checked_mul(copies)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RotateDirection {
    Left,
    Right,
}

#[derive(Clone, Copy)]
enum EmissionAction {
    Original,
    RotateImmediate {
        destination: usize,
        source: usize,
        direction: RotateDirection,
        count: u8,
    },
    ElideDeadShift,
}

#[derive(Clone, Copy)]
#[cfg_attr(
    not(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    )),
    allow(dead_code)
)]
pub(crate) enum OptimizationEventKind {
    FusedRotate,
    ElidedShift,
}

#[derive(Clone, Copy)]
#[cfg_attr(
    not(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    )),
    allow(dead_code)
)]
pub(crate) struct OptimizationEvent {
    pub(crate) retired_offset: usize,
    pub(crate) kind: OptimizationEventKind,
}

impl EmissionAction {
    const fn register_usage(self, original: Lowering) -> RegisterUsage {
        match self {
            Self::Original => original.register_usage(),
            Self::RotateImmediate { destination: 0, .. } | Self::ElideDeadShift => RegisterUsage {
                reads: [None, None],
                write: None,
            },
            Self::RotateImmediate {
                destination,
                source,
                ..
            } => RegisterUsage {
                reads: [Some(source), None],
                write: Some(destination),
            },
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct ValueId(usize);

#[derive(Clone, Copy)]
struct PureNode {
    reads: [Option<ValueId>; 2],
    written: Option<ValueId>,
}

#[derive(Clone, Copy)]
struct ShiftDefinition {
    value: ValueId,
    source: usize,
    source_value: ValueId,
    direction: RotateDirection,
    count: u8,
}

#[derive(Clone, Copy)]
struct RotateCandidate {
    instruction_index: usize,
    destination: usize,
    source: usize,
    direction: RotateDirection,
    count: u8,
    shift_values: [ValueId; 2],
}

struct PureSpanPlan {
    actions: Vec<EmissionAction>,
    fused_rotates: usize,
    elided_shifts: usize,
}

impl PureSpanPlan {
    fn analyze(instructions: &[PreparedInstruction]) -> Self {
        let mut plan = Self {
            actions: vec![EmissionAction::Original; instructions.len()],
            fused_rotates: 0,
            elided_shifts: 0,
        };
        let mut start = 0;
        while start < instructions.len() {
            while start < instructions.len() && !is_pure(instructions[start].lowering) {
                start += 1;
            }
            let mut end = start;
            while end < instructions.len() && is_pure(instructions[end].lowering) {
                end += 1;
            }
            if start < end && span_may_contain_rotate(&instructions[start..end]) {
                plan.analyze_span(instructions, start, end);
            }
            start = end;
        }
        plan
    }

    fn analyze_span(&mut self, instructions: &[PreparedInstruction], start: usize, end: usize) {
        let mut current: [ValueId; 32] = std::array::from_fn(ValueId);
        let mut definitions = vec![None; 32];
        let mut uses = vec![0_usize; 32];
        let mut nodes = Vec::with_capacity(end - start);
        let mut candidates = Vec::new();

        for instruction_index in start..end {
            let lowering = instructions[instruction_index].lowering;
            let usage = lowering.register_usage();
            let mut reads = [None; 2];
            for (slot, register) in usage.reads.into_iter().enumerate() {
                let Some(register) = register else {
                    continue;
                };
                let value = current[register];
                reads[slot] = Some(value);
                uses[value.0] += 1;
            }

            if let Lowering::Register {
                destination,
                operation: RegisterOperation::Or,
                ..
            } = lowering
                && destination != 0
                && let (Some(left), Some(right)) = (reads[0], reads[1])
                && left != right
                && let (Some(left), Some(right)) = (
                    shift_definition(
                        instructions,
                        start,
                        &nodes,
                        &definitions,
                        left,
                    ),
                    shift_definition(
                        instructions,
                        start,
                        &nodes,
                        &definitions,
                        right,
                    ),
                )
                && left.direction != right.direction
                && left.source == right.source
                && left.source_value == right.source_value
                && left.count != 0
                && right.count != 0
                && u16::from(left.count) + u16::from(right.count) == 32
                // The synthesized rotate reads the architectural source at
                // the OR site. Require that exact SSA value to remain there;
                // this conservatively rejects source/temp aliasing and source
                // clobbers while still permitting unrelated pure interleaving.
                && current[left.source] == left.source_value
            {
                let (direction, count) = if left.direction == RotateDirection::Right {
                    choose_rotate(left.count, right.count)
                } else {
                    choose_rotate(right.count, left.count)
                };
                candidates.push(RotateCandidate {
                    instruction_index,
                    destination,
                    source: left.source,
                    direction,
                    count,
                    shift_values: [left.value, right.value],
                });
            }

            let written = usage
                .write
                .filter(|&register| register != 0)
                .map(|register| {
                    let value = ValueId(definitions.len());
                    definitions.push(Some(nodes.len()));
                    uses.push(0);
                    current[register] = value;
                    value
                });
            nodes.push(PureNode { reads, written });
        }

        let mut live_out = vec![false; definitions.len()];
        for value in current.into_iter().skip(1) {
            live_out[value.0] = true;
        }
        let mut potential_replaced_uses = vec![0_usize; definitions.len()];
        for candidate in &candidates {
            for value in candidate.shift_values {
                potential_replaced_uses[value.0] += 1;
            }
        }
        let potentially_elidable = (0..definitions.len())
            .map(|value| {
                potential_replaced_uses[value] != 0
                    && potential_replaced_uses[value] == uses[value]
                    && !live_out[value]
            })
            .collect::<Vec<_>>();
        let mut replaced_uses = vec![0_usize; definitions.len()];
        for candidate in candidates {
            // A rotate is profitable only when it makes at least one producer
            // write unobservable. Retaining both shifts and replacing just the
            // OR can add source traffic without reducing emitted operations.
            if !candidate
                .shift_values
                .into_iter()
                .any(|value| potentially_elidable[value.0])
            {
                continue;
            }
            debug_assert!(matches!(
                self.actions[candidate.instruction_index],
                EmissionAction::Original
            ));
            self.actions[candidate.instruction_index] = EmissionAction::RotateImmediate {
                destination: candidate.destination,
                source: candidate.source,
                direction: candidate.direction,
                count: candidate.count,
            };
            self.fused_rotates += 1;
            for value in candidate.shift_values {
                replaced_uses[value.0] += 1;
            }
        }

        for value in 32..definitions.len() {
            if replaced_uses[value] == 0 || replaced_uses[value] != uses[value] || live_out[value] {
                continue;
            }
            let relative = definitions[value].expect("written value has a definition");
            let instruction_index = start + relative;
            debug_assert!(matches!(
                instructions[instruction_index].lowering,
                Lowering::Immediate {
                    operation: ImmediateOperation::ShiftLeft(_) | ImmediateOperation::ShiftRight(_),
                    ..
                }
            ));
            self.actions[instruction_index] = EmissionAction::ElideDeadShift;
            self.elided_shifts += 1;
        }
    }

    fn register_usage(&self, index: usize, original: Lowering) -> RegisterUsage {
        self.actions[index].register_usage(original)
    }

    fn optimization_events(&self) -> Vec<OptimizationEvent> {
        self.actions
            .iter()
            .enumerate()
            .filter_map(|(retired_offset, action)| {
                let kind = match action {
                    EmissionAction::Original => return None,
                    EmissionAction::RotateImmediate { .. } => OptimizationEventKind::FusedRotate,
                    EmissionAction::ElideDeadShift => OptimizationEventKind::ElidedShift,
                };
                Some(OptimizationEvent {
                    retired_offset,
                    kind,
                })
            })
            .collect()
    }
}

fn span_may_contain_rotate(instructions: &[PreparedInstruction]) -> bool {
    let mut shift_left = false;
    let mut shift_right = false;
    let mut or = false;
    for prepared in instructions {
        match prepared.lowering {
            Lowering::Immediate {
                operation: ImmediateOperation::ShiftLeft(count),
                ..
            } => shift_left |= count != 0,
            Lowering::Immediate {
                operation: ImmediateOperation::ShiftRight(count),
                ..
            } => shift_right |= count != 0,
            Lowering::Register {
                destination,
                operation: RegisterOperation::Or,
                ..
            } => or |= destination != 0,
            _ => {}
        }
        if shift_left && shift_right && or {
            return true;
        }
    }
    false
}

fn is_pure(lowering: Lowering) -> bool {
    match lowering {
        Lowering::WriteImmediate { .. } | Lowering::Immediate { .. } => true,
        Lowering::Register { operation, .. } => !matches!(
            operation,
            RegisterOperation::Divide
                | RegisterOperation::DivideUnsigned
                | RegisterOperation::Remainder
                | RegisterOperation::RemainderUnsigned
        ),
        Lowering::Jump { .. }
        | Lowering::JumpRegister { .. }
        | Lowering::Branch { .. }
        | Lowering::Load { .. }
        | Lowering::Store { .. }
        | Lowering::Fence => false,
    }
}

fn shift_definition(
    instructions: &[PreparedInstruction],
    span_start: usize,
    nodes: &[PureNode],
    definitions: &[Option<usize>],
    value: ValueId,
) -> Option<ShiftDefinition> {
    let relative = *definitions.get(value.0)?.as_ref()?;
    let instruction_index = span_start.checked_add(relative)?;
    let node = nodes.get(relative)?;
    let Lowering::Immediate {
        destination: _,
        source,
        operation,
    } = instructions.get(instruction_index)?.lowering
    else {
        return None;
    };
    let (direction, count) = match operation {
        ImmediateOperation::ShiftLeft(count) => (RotateDirection::Left, count),
        ImmediateOperation::ShiftRight(count) => (RotateDirection::Right, count),
        _ => return None,
    };
    Some(ShiftDefinition {
        value: node.written.filter(|&written| written == value)?,
        source,
        source_value: node.reads[0]?,
        direction,
        count,
    })
}

fn choose_rotate(right_count: u8, left_count: u8) -> (RotateDirection, u8) {
    if right_count <= left_count {
        (RotateDirection::Right, right_count)
    } else {
        (RotateDirection::Left, left_count)
    }
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
enum RegisterOperand {
    Zero,
    Host(CacheHost),
    Canonical(u8),
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
    fn analyze(instructions: &[PreparedInstruction], emissions: &PureSpanPlan) -> Self {
        Self::analyze_with_mode(instructions, emissions, RegisterPlanningMode::Bounded)
    }

    fn analyze_loop(instructions: &[PreparedInstruction], emissions: &PureSpanPlan) -> Self {
        Self::analyze_with_mode(instructions, emissions, RegisterPlanningMode::Loop)
    }

    fn analyze_with_mode(
        instructions: &[PreparedInstruction],
        emissions: &PureSpanPlan,
        mode: RegisterPlanningMode,
    ) -> Self {
        let mut scores = [RegisterScore::default(); 32];
        let mut access_index = 0;
        let mut uncached_accesses = 0_usize;

        for (index, prepared) in instructions.iter().enumerate() {
            let usage = emissions.register_usage(index, prepared.lowering);
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
            .enumerate()
            .flat_map(|(index, prepared)| emissions.register_usage(index, prepared.lowering).reads)
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
    fused_rotate_count: usize,
    elided_shift_count: usize,
    #[cfg_attr(
        not(all(
            target_arch = "x86_64",
            target_os = "linux",
            target_pointer_width = "64"
        )),
        allow(dead_code)
    )]
    pub(crate) optimization_events: Vec<OptimizationEvent>,
    memory_checks: MemoryCheckStats,
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

    /// Number of complementary-shift ORs fused in one logical guest path or
    /// loop cycle. Grouped loops retain the per-cycle count.
    pub const fn fused_rotate_count(&self) -> usize {
        self.fused_rotate_count
    }

    /// Number of dead shift writes elided in one logical guest path or loop
    /// cycle. Grouped loops retain the per-cycle count.
    pub const fn elided_shift_count(&self) -> usize {
        self.elided_shift_count
    }

    /// Number of emitted guest load and store instructions.
    pub const fn memory_accesses(&self) -> usize {
        self.memory_checks.accesses
    }

    /// Number of non-byte accesses that would ordinarily require alignment guards.
    pub const fn memory_alignment_candidates(&self) -> usize {
        self.memory_checks.alignment_candidates
    }

    /// Number of alignment guards retained after local proof reuse.
    pub const fn memory_alignment_checks(&self) -> usize {
        self.memory_checks.alignment_checks
    }

    /// Number of permission/range guards retained after local proof reuse.
    pub const fn memory_permission_checks(&self) -> usize {
        self.memory_checks.permission_checks
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
                memory_validation: MemoryValidation::FULL,
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

    plan_memory_validations(&mut prepared);
    let instruction_count = prepared.len();
    let emissions = PureSpanPlan::analyze(&prepared);
    let plan = if closes_loop {
        RegisterPlan::analyze_loop(&prepared, &emissions)
    } else {
        RegisterPlan::analyze(&prepared, &emissions)
    };
    let uncached_register_accesses = plan.uncached_accesses;
    let cached_register_accesses = plan.cached_accesses;
    let memory_checks = MemoryCheckStats::for_instructions(&prepared);

    if closes_loop {
        debug_assert!(!final_returned);
        return emit_counted_loop(&prepared, &emissions, plan, starts[0], loop_group_factor);
    }

    let mut emitter = Emitter::new(plan);
    for (retired, prepared) in prepared.into_iter().enumerate() {
        let preferred_successor = prepared.preferred_successor;
        let flow = emitter.emission(
            prepared.instruction,
            prepared.lowering,
            emissions.actions[retired],
            prepared.memory_validation,
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
        fused_rotate_count: emissions.fused_rotates,
        elided_shift_count: emissions.elided_shifts,
        optimization_events: emissions.optimization_events(),
        memory_checks,
    })
}

fn emit_counted_loop(
    prepared: &[PreparedInstruction],
    emissions: &PureSpanPlan,
    plan: RegisterPlan,
    start: u32,
    unroll_factor: usize,
) -> Option<CompiledBlock> {
    let instruction_count = prepared.len();
    let minimum_instruction_count = instruction_count.checked_mul(unroll_factor)?;
    let uncached_register_accesses = plan.uncached_accesses;
    let cached_register_accesses = plan.cached_accesses;
    let memory_checks = MemoryCheckStats::for_instructions(prepared).repeated(unroll_factor)?;
    let mut emitter = Emitter::new_loop(plan, minimum_instruction_count)?;
    let loop_start = emitter.code.len();

    for copy in 0..unroll_factor {
        let retirement_base = copy.checked_mul(instruction_count)?;
        for (offset, prepared) in prepared.iter().copied().enumerate() {
            let retired = retirement_base.checked_add(offset)?;
            let flow = emitter.emission(
                prepared.instruction,
                prepared.lowering,
                emissions.actions[offset],
                prepared.memory_validation,
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
        fused_rotate_count: emissions.fused_rotates,
        elided_shift_count: emissions.elided_shifts,
        optimization_events: emissions.optimization_events(),
        memory_checks,
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

fn plan_memory_validations(instructions: &mut [PreparedInstruction]) {
    let mut values = std::array::from_fn(|register| {
        if register == 0 {
            SymbolicValue::Constant(0)
        } else {
            SymbolicValue::Affine {
                root: register as u16,
                offset: 0,
            }
        }
    });
    let mut next_root = 32_u16;
    let mut permission_proofs = Vec::<PermissionProof>::new();
    let mut alignment_proofs = Vec::<AlignmentProof>::new();

    for prepared in instructions {
        let memory = match prepared.lowering {
            Lowering::Load {
                source,
                offset,
                width,
                ..
            } => Some((source, offset, width, PERM_READ)),
            Lowering::Store {
                base,
                offset,
                width,
                ..
            } => Some((base, offset, width, PERM_WRITE)),
            _ => None,
        };
        if let Some((base, offset, width, permission)) = memory {
            let address = values[base].wrapping_add(offset);
            let bytes = width.bytes();
            let alignment = bytes != 1
                && !matches!(
                    address,
                    SymbolicValue::Constant(address) if address.is_multiple_of(bytes)
                )
                && !alignment_proofs
                    .iter()
                    .any(|proof| proof_implies_alignment(*proof, address, bytes));
            let page = ProvenPage::for_address(address);
            let permission_required = !permission_proofs
                .iter()
                .any(|proof| proof.page == page && proof.permission == permission);
            prepared.memory_validation = MemoryValidation {
                alignment,
                permission: permission_required,
            };

            if let Some(proof) = alignment_proofs
                .iter_mut()
                .find(|proof| proof.address == address)
            {
                proof.bytes = proof.bytes.max(bytes);
            } else {
                alignment_proofs.push(AlignmentProof { address, bytes });
            }
            if permission_required {
                permission_proofs.push(PermissionProof { page, permission });
            }
        }

        update_symbolic_values(&mut values, &mut next_root, prepared.lowering);
    }
}

fn proof_implies_alignment(proof: AlignmentProof, address: SymbolicValue, bytes: u32) -> bool {
    if proof.bytes < bytes {
        return false;
    }
    match (proof.address, address) {
        (SymbolicValue::Constant(_), SymbolicValue::Constant(address)) => {
            address.is_multiple_of(bytes)
        }
        (
            SymbolicValue::Affine {
                root: proof_root,
                offset: proof_offset,
            },
            SymbolicValue::Affine { root, offset },
        ) => proof_root == root && offset.wrapping_sub(proof_offset).is_multiple_of(bytes),
        _ => false,
    }
}

fn update_symbolic_values(
    values: &mut [SymbolicValue; 32],
    next_root: &mut u16,
    lowering: Lowering,
) {
    let write = match lowering {
        Lowering::WriteImmediate { destination, value }
        | Lowering::Jump {
            destination,
            link: value,
            ..
        }
        | Lowering::JumpRegister {
            destination,
            link: value,
            ..
        } => Some((destination, SymbolicValue::Constant(value))),
        Lowering::Immediate {
            destination,
            source,
            operation,
        } => Some((
            destination,
            symbolic_immediate(values[source], operation)
                .unwrap_or_else(|| fresh_symbolic(next_root)),
        )),
        Lowering::Register {
            destination,
            left,
            right,
            operation,
        } => Some((
            destination,
            symbolic_register(values[left], values[right], operation)
                .unwrap_or_else(|| fresh_symbolic(next_root)),
        )),
        Lowering::Load { destination, .. } => Some((destination, fresh_symbolic(next_root))),
        Lowering::Branch { .. } | Lowering::Store { .. } | Lowering::Fence => None,
    };
    if let Some((destination, value)) = write.filter(|(destination, _)| *destination != 0) {
        values[destination] = value;
    }
}

fn fresh_symbolic(next_root: &mut u16) -> SymbolicValue {
    let root = *next_root;
    *next_root = next_root
        .checked_add(1)
        .expect("bounded native compilation cannot exhaust symbolic roots");
    SymbolicValue::Affine { root, offset: 0 }
}

fn symbolic_immediate(
    source: SymbolicValue,
    operation: ImmediateOperation,
) -> Option<SymbolicValue> {
    match operation {
        ImmediateOperation::Add(value) => Some(source.wrapping_add(value)),
        ImmediateOperation::Xor(0) | ImmediateOperation::Or(0) => Some(source),
        ImmediateOperation::And(u32::MAX) => Some(source),
        _ => {
            let SymbolicValue::Constant(source) = source else {
                return None;
            };
            let value = match operation {
                ImmediateOperation::Add(value) => source.wrapping_add(value),
                ImmediateOperation::SetLessThan(value) => {
                    u32::from((source as i32) < (value as i32))
                }
                ImmediateOperation::SetBelow(value) => u32::from(source < value),
                ImmediateOperation::Xor(value) => source ^ value,
                ImmediateOperation::Or(value) => source | value,
                ImmediateOperation::And(value) => source & value,
                ImmediateOperation::ShiftLeft(count) => source.wrapping_shl(u32::from(count)),
                ImmediateOperation::ShiftRight(count) => source.wrapping_shr(u32::from(count)),
                ImmediateOperation::ShiftRightArithmetic(count) => {
                    (source as i32).wrapping_shr(u32::from(count)) as u32
                }
            };
            Some(SymbolicValue::Constant(value))
        }
    }
}

fn symbolic_register(
    left: SymbolicValue,
    right: SymbolicValue,
    operation: RegisterOperation,
) -> Option<SymbolicValue> {
    if left == right {
        return match operation {
            RegisterOperation::Subtract | RegisterOperation::Xor => {
                Some(SymbolicValue::Constant(0))
            }
            RegisterOperation::Or | RegisterOperation::And => Some(left),
            _ => None,
        };
    }
    match (left, right, operation) {
        (left, SymbolicValue::Constant(0), RegisterOperation::Add)
        | (SymbolicValue::Constant(0), left, RegisterOperation::Add)
        | (left, SymbolicValue::Constant(0), RegisterOperation::Subtract)
        | (left, SymbolicValue::Constant(0), RegisterOperation::Xor)
        | (SymbolicValue::Constant(0), left, RegisterOperation::Xor)
        | (left, SymbolicValue::Constant(0), RegisterOperation::Or)
        | (SymbolicValue::Constant(0), left, RegisterOperation::Or) => Some(left),
        (left, SymbolicValue::Constant(value), RegisterOperation::Add)
        | (SymbolicValue::Constant(value), left, RegisterOperation::Add) => {
            Some(left.wrapping_add(value))
        }
        (left, SymbolicValue::Constant(value), RegisterOperation::Subtract) => {
            Some(left.wrapping_add(value.wrapping_neg()))
        }
        (left, SymbolicValue::Constant(u32::MAX), RegisterOperation::And)
        | (SymbolicValue::Constant(u32::MAX), left, RegisterOperation::And) => Some(left),
        (SymbolicValue::Constant(left), SymbolicValue::Constant(right), operation) => {
            let value = match operation {
                RegisterOperation::Add => left.wrapping_add(right),
                RegisterOperation::Subtract => left.wrapping_sub(right),
                RegisterOperation::ShiftLeft => left.wrapping_shl(right & 31),
                RegisterOperation::SetLessThan => u32::from((left as i32) < (right as i32)),
                RegisterOperation::SetBelow => u32::from(left < right),
                RegisterOperation::Xor => left ^ right,
                RegisterOperation::ShiftRight => left.wrapping_shr(right & 31),
                RegisterOperation::ShiftRightArithmetic => {
                    (left as i32).wrapping_shr(right & 31) as u32
                }
                RegisterOperation::Or => left | right,
                RegisterOperation::And => left & right,
                _ => return None,
            };
            Some(SymbolicValue::Constant(value))
        }
        _ => None,
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

    fn emission(
        &mut self,
        instruction: DecodedInstruction,
        lowering: Lowering,
        action: EmissionAction,
        memory_validation: MemoryValidation,
        retired: usize,
        preferred_successor: Option<u32>,
    ) -> Option<Flow> {
        match action {
            EmissionAction::Original => self.instruction(
                instruction,
                lowering,
                memory_validation,
                retired,
                preferred_successor,
            ),
            EmissionAction::RotateImmediate {
                destination,
                source,
                direction,
                count,
            } => {
                debug_assert!(matches!(
                    lowering,
                    Lowering::Register {
                        destination: original_destination,
                        operation: RegisterOperation::Or,
                        ..
                    } if original_destination == destination
                ));
                self.rotate_immediate(destination, source, direction, count);
                Some(Flow::Continue)
            }
            EmissionAction::ElideDeadShift => {
                debug_assert!(matches!(
                    lowering,
                    Lowering::Immediate {
                        operation: ImmediateOperation::ShiftLeft(_)
                            | ImmediateOperation::ShiftRight(_),
                        ..
                    }
                ));
                // The prepared instruction and its retirement index remain in
                // place. Only its host write is omitted after SSA liveness
                // proves the value unobservable at every following barrier.
                Some(Flow::Continue)
            }
        }
    }

    fn instruction(
        &mut self,
        instruction: DecodedInstruction,
        lowering: Lowering,
        memory_validation: MemoryValidation,
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
                memory_validation,
            ),
            Lowering::Store {
                source,
                base,
                offset,
                width,
            } => self.store(
                instruction.pc(),
                retired,
                source,
                base,
                offset,
                width,
                memory_validation,
            ),
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
        if offset != 0 {
            self.eax_alu_immediate(0, 0x05, offset);
        }
        self.eax_alu_immediate(4, 0x25, !1);
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
            ImmediateOperation::Add(value) => self.eax_alu_immediate(0, 0x05, value),
            ImmediateOperation::Xor(value) => self.eax_alu_immediate(6, 0x35, value),
            ImmediateOperation::Or(value) => self.eax_alu_immediate(1, 0x0d, value),
            ImmediateOperation::And(value) => self.eax_alu_immediate(4, 0x25, value),
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

    fn rotate_immediate(
        &mut self,
        destination: usize,
        source: usize,
        direction: RotateDirection,
        count: u8,
    ) {
        debug_assert!(destination != 0);
        debug_assert!((1..32).contains(&count));

        // Resolve the source before a cached destination can overwrite it.
        // This is required when the final OR destination aliases the source
        // or either eliminated shift temporary.
        let source = self.register_operand(source);
        if let Some(slot) = self.cache_index(destination) {
            let host = self.cache[slot].expect("cache slot exists").register.host;
            if !matches!(source, RegisterOperand::Host(source_host) if source_host.code() == host.code())
            {
                self.move_host_operand(host, source);
            }
            self.host_rotate_immediate(host, direction, count);
            self.mark_cache_written(slot);
            return;
        }

        self.move_eax_operand(source);
        self.eax_rotate_immediate(direction, count);
        self.store_eax(destination);
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

        if matches!(
            operation,
            RegisterOperation::Add
                | RegisterOperation::Subtract
                | RegisterOperation::Xor
                | RegisterOperation::Or
                | RegisterOperation::And
                | RegisterOperation::Multiply
        ) {
            self.simple_register(destination, left, right, operation);
            return Some(Flow::Continue);
        }

        if destination != 0
            && let Some(slot) = self.cache_index(destination)
        {
            let direct = matches!(
                operation,
                RegisterOperation::ShiftLeft
                    | RegisterOperation::ShiftRight
                    | RegisterOperation::ShiftRightArithmetic
            );
            let other = if destination == left {
                Some(right)
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

    fn simple_register(
        &mut self,
        destination: usize,
        left: usize,
        right: usize,
        operation: RegisterOperation,
    ) {
        debug_assert!(destination != 0);
        // Resolve and preload both sources before a cached destination can
        // overwrite either one; this is the aliasing invariant for this path.
        let left_operand = self.register_operand(left);
        let right_operand = self.register_operand(right);

        if let Some(slot) = self.cache_index(destination) {
            let host = self.cache[slot].expect("cache slot exists").register.host;
            let source = if destination == left {
                Some(right_operand)
            } else if destination == right && is_commutative(operation) {
                Some(left_operand)
            } else if destination != right {
                self.move_host_operand(host, left_operand);
                Some(right_operand)
            } else {
                None
            };
            if let Some(source) = source {
                self.host_register_operand(host, source, operation);
                self.mark_cache_written(slot);
                return;
            }
        }

        self.move_eax_operand(left_operand);
        self.eax_register_operand(right_operand, operation);
        self.store_eax(destination);
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

        let left = self.register_operand(left);
        let right = self.register_operand(right);
        // Keep the flag producer adjacent to the branch below. The operand
        // resolver may emit lazy cache loads, so both operands come first.
        self.compare_register_operands(left, right);
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
        validation: MemoryValidation,
    ) -> Option<Flow> {
        self.memory_prefix(source, offset, width, PERM_READ, pc, retired, validation)?;
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
        validation: MemoryValidation,
    ) -> Option<Flow> {
        self.memory_prefix(base, offset, width, PERM_WRITE, pc, retired, validation)?;
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

    #[allow(clippy::too_many_arguments)]
    fn memory_prefix(
        &mut self,
        base: usize,
        offset: u32,
        width: MemoryWidth,
        permission: u8,
        pc: u32,
        retired: usize,
        validation: MemoryValidation,
    ) -> Option<()> {
        self.load_eax(base);
        if offset != 0 {
            self.eax_alu_immediate(0, 0x05, offset);
        }
        let bytes = width.bytes();
        if validation.alignment && bytes != 1 {
            self.code.push(0xa9);
            self.code.extend_from_slice(&(bytes - 1).to_le_bytes());
            self.side_exit_conditional(0x85, pc, retired)?;
        }

        if validation.permission {
            // Naturally aligned byte, halfword, and word accesses cannot cross a
            // 4 KiB guest page, so checking the first page is sufficient. The
            // permission table has a permanent zero guard entry for every RV32
            // page outside the architectural address space, making this check the
            // range guard as well. The complete guest address remains in eax for
            // direct flat-memory access after the check passes.
            self.code
                .extend_from_slice(&[0x89, 0xc1, 0xc1, 0xe9, PAGE_SHIFT as u8]);
            self.code.extend_from_slice(&[0xf6, 0x04, 0x0e, permission]);
            self.side_exit_conditional(0x84, pc, retired)?;
        }
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

    fn register_operand(&mut self, register: usize) -> RegisterOperand {
        if register == 0 {
            RegisterOperand::Zero
        } else if let Some(host) = self.cached_host_for_read(register) {
            RegisterOperand::Host(host)
        } else {
            RegisterOperand::Canonical(register_offset(register))
        }
    }

    fn compare_register_operands(&mut self, left: RegisterOperand, right: RegisterOperand) {
        match (left, right) {
            (RegisterOperand::Zero, RegisterOperand::Zero) => {
                self.code.extend_from_slice(&[0x31, 0xc0]);
            }
            (RegisterOperand::Host(left), RegisterOperand::Zero) => {
                let left = left.code() & 7;
                self.code
                    .extend_from_slice(&[0x45, 0x85, 0xc0 | (left << 3) | left]);
            }
            (RegisterOperand::Canonical(left), RegisterOperand::Zero) => {
                self.code.extend_from_slice(&[0x83, 0x7f, left, 0x00]);
            }
            (RegisterOperand::Zero, RegisterOperand::Host(right)) => {
                self.code.extend_from_slice(&[0x31, 0xc0]);
                self.code
                    .extend_from_slice(&[0x41, 0x3b, 0xc0 | (right.code() & 7)]);
            }
            (RegisterOperand::Zero, RegisterOperand::Canonical(right)) => {
                self.code
                    .extend_from_slice(&[0x31, 0xc0, 0x3b, 0x47, right]);
            }
            (RegisterOperand::Host(left), RegisterOperand::Host(right)) => {
                self.code.extend_from_slice(&[
                    0x45,
                    0x39,
                    0xc0 | ((right.code() & 7) << 3) | (left.code() & 7),
                ]);
            }
            (RegisterOperand::Host(left), RegisterOperand::Canonical(right)) => {
                self.code
                    .extend_from_slice(&[0x44, 0x3b, 0x47 | ((left.code() & 7) << 3), right]);
            }
            (RegisterOperand::Canonical(left), RegisterOperand::Host(right)) => {
                self.code
                    .extend_from_slice(&[0x44, 0x39, 0x47 | ((right.code() & 7) << 3), left]);
            }
            (RegisterOperand::Canonical(left), RegisterOperand::Canonical(right)) => {
                self.code
                    .extend_from_slice(&[0x8b, 0x47, left, 0x3b, 0x47, right]);
            }
        }
    }

    fn move_eax_operand(&mut self, source: RegisterOperand) {
        match source {
            RegisterOperand::Zero => self.code.extend_from_slice(&[0x31, 0xc0]),
            RegisterOperand::Host(source) => self.mov_eax_host(source),
            RegisterOperand::Canonical(source) => {
                self.code.extend_from_slice(&[0x8b, 0x47, source]);
            }
        }
    }

    fn move_host_operand(&mut self, host: CacheHost, source: RegisterOperand) {
        let host = host.code() & 7;
        match source {
            RegisterOperand::Zero => {
                self.code
                    .extend_from_slice(&[0x45, 0x31, 0xc0 | (host << 3) | host])
            }
            RegisterOperand::Host(source) => {
                self.code
                    .extend_from_slice(&[0x45, 0x89, 0xc0 | ((source.code() & 7) << 3) | host])
            }
            RegisterOperand::Canonical(source) => {
                self.code
                    .extend_from_slice(&[0x44, 0x8b, 0x47 | (host << 3), source])
            }
        }
    }

    fn eax_register_operand(&mut self, source: RegisterOperand, operation: RegisterOperation) {
        match source {
            RegisterOperand::Zero => self.zero_eax_for_operation(operation),
            RegisterOperand::Host(source) => {
                let source = source.code() & 7;
                if matches!(operation, RegisterOperation::Multiply) {
                    self.code
                        .extend_from_slice(&[0x41, 0x0f, 0xaf, 0xc0 | source]);
                } else {
                    self.code.extend_from_slice(&[
                        0x41,
                        register_source_opcode(operation),
                        0xc0 | source,
                    ]);
                }
            }
            RegisterOperand::Canonical(source) => {
                if matches!(operation, RegisterOperation::Multiply) {
                    self.code.extend_from_slice(&[0x0f, 0xaf, 0x47, source]);
                } else {
                    self.code
                        .extend_from_slice(&[register_source_opcode(operation), 0x47, source]);
                }
            }
        }
    }

    fn host_register_operand(
        &mut self,
        host: CacheHost,
        source: RegisterOperand,
        operation: RegisterOperation,
    ) {
        let host = host.code() & 7;
        match source {
            RegisterOperand::Zero => self.zero_host_for_operation(host, operation),
            RegisterOperand::Host(source) => {
                let source = source.code() & 7;
                if matches!(operation, RegisterOperation::Multiply) {
                    self.code
                        .extend_from_slice(&[0x45, 0x0f, 0xaf, 0xc0 | (host << 3) | source]);
                } else {
                    self.code.extend_from_slice(&[
                        0x45,
                        register_destination_opcode(operation),
                        0xc0 | (source << 3) | host,
                    ]);
                }
            }
            RegisterOperand::Canonical(source) => {
                if matches!(operation, RegisterOperation::Multiply) {
                    self.code
                        .extend_from_slice(&[0x44, 0x0f, 0xaf, 0x47 | (host << 3), source]);
                } else {
                    self.code.extend_from_slice(&[
                        0x44,
                        register_source_opcode(operation),
                        0x47 | (host << 3),
                        source,
                    ]);
                }
            }
        }
    }

    fn zero_eax_for_operation(&mut self, operation: RegisterOperation) {
        if matches!(
            operation,
            RegisterOperation::And | RegisterOperation::Multiply
        ) {
            self.code.extend_from_slice(&[0x31, 0xc0]);
        } else {
            debug_assert!(matches!(
                operation,
                RegisterOperation::Add
                    | RegisterOperation::Subtract
                    | RegisterOperation::Xor
                    | RegisterOperation::Or
            ));
        }
    }

    fn zero_host_for_operation(&mut self, host: u8, operation: RegisterOperation) {
        if matches!(
            operation,
            RegisterOperation::And | RegisterOperation::Multiply
        ) {
            self.code
                .extend_from_slice(&[0x45, 0x31, 0xc0 | (host << 3) | host]);
        } else {
            debug_assert!(matches!(
                operation,
                RegisterOperation::Add
                    | RegisterOperation::Subtract
                    | RegisterOperation::Xor
                    | RegisterOperation::Or
            ));
        }
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
        let operand = 0xc0 | (extension << 3) | (host.code() & 7);
        if let Some(value) = sign_extended_imm8(value) {
            self.code.extend_from_slice(&[0x41, 0x83, operand, value]);
        } else {
            self.code.extend_from_slice(&[0x41, 0x81, operand]);
            self.code.extend_from_slice(&value.to_le_bytes());
        }
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

    fn eax_alu_immediate(&mut self, extension: u8, opcode: u8, value: u32) {
        if let Some(value) = sign_extended_imm8(value) {
            self.code
                .extend_from_slice(&[0x83, 0xc0 | (extension << 3), value]);
        } else {
            self.code.push(opcode);
            self.code.extend_from_slice(&value.to_le_bytes());
        }
    }

    fn eax_shift(&mut self, extension: u8, count: u8) {
        self.code.extend_from_slice(&[0xc1, extension, count]);
    }

    fn eax_rotate_immediate(&mut self, direction: RotateDirection, count: u8) {
        let extension = match direction {
            RotateDirection::Left => 0,
            RotateDirection::Right => 1,
        };
        self.code
            .extend_from_slice(&[0xc1, 0xc0 | (extension << 3), count]);
    }

    fn host_rotate_immediate(&mut self, host: CacheHost, direction: RotateDirection, count: u8) {
        let extension = match direction {
            RotateDirection::Left => 0,
            RotateDirection::Right => 1,
        };
        self.code.extend_from_slice(&[
            0x41,
            0xc1,
            0xc0 | (extension << 3) | (host.code() & 7),
            count,
        ]);
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

const fn is_commutative(operation: RegisterOperation) -> bool {
    matches!(
        operation,
        RegisterOperation::Add
            | RegisterOperation::Xor
            | RegisterOperation::Or
            | RegisterOperation::And
            | RegisterOperation::Multiply
    )
}

const fn register_destination_opcode(operation: RegisterOperation) -> u8 {
    match operation {
        RegisterOperation::Add => 0x01,
        RegisterOperation::Subtract => 0x29,
        RegisterOperation::Xor => 0x31,
        RegisterOperation::Or => 0x09,
        RegisterOperation::And => 0x21,
        _ => unreachable!(),
    }
}

const fn register_source_opcode(operation: RegisterOperation) -> u8 {
    match operation {
        RegisterOperation::Add => 0x03,
        RegisterOperation::Subtract => 0x2b,
        RegisterOperation::Xor => 0x33,
        RegisterOperation::Or => 0x0b,
        RegisterOperation::And => 0x23,
        _ => unreachable!(),
    }
}

fn sign_extended_imm8(value: u32) -> Option<u8> {
    let byte = value as u8;
    ((byte as i8 as i32 as u32) == value).then_some(byte)
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

    fn shift_immediate(rd: u32, rs1: u32, funct3: u32, count: u32) -> u32 {
        ((count & 31) << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x13
    }

    fn sw(rs2: u32, rs1: u32, immediate: i32) -> u32 {
        let immediate = immediate as u32 & 0xfff;
        ((immediate >> 5) << 25)
            | (rs2 << 20)
            | (rs1 << 15)
            | (2 << 12)
            | ((immediate & 0x1f) << 7)
            | 0x23
    }

    fn decoded(machine: &Machine, start: u32, count: usize) -> Vec<BlockInstruction> {
        (0..count)
            .map(|index| machine.fetch_decode(start + index as u32 * 4))
            .collect()
    }

    fn contains_bytes(code: &[u8], bytes: &[u8]) -> bool {
        code.windows(bytes.len()).any(|window| window == bytes)
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
    fn reuses_only_dominating_exact_memory_validation_proofs() {
        let machine = machine_with_code(&[lw(5, 6, 0), lw(0, 6, 0), lw(7, 6, 4)], IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();

        assert_eq!(compiled.memory_accesses(), 3);
        assert_eq!(compiled.memory_alignment_candidates(), 3);
        // The first aligned address also proves alignment at a congruent
        // offset, but only the exact repeated address proves its page.
        assert_eq!(compiled.memory_alignment_checks(), 1);
        assert_eq!(compiled.memory_permission_checks(), 2);

        let machine = machine_with_code(&[lw(6, 6, 0), lw(5, 6, 0)], IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        // The first load changes the base value, so neither proof reaches the
        // second dynamic address.
        assert_eq!(compiled.memory_alignment_checks(), 2);
        assert_eq!(compiled.memory_permission_checks(), 2);
    }

    #[test]
    fn permission_proofs_do_not_cross_read_write_kinds() {
        let machine = machine_with_code(&[sw(5, 6, 0), sw(7, 6, 0), lw(8, 6, 0)], IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();

        assert_eq!(compiled.memory_accesses(), 3);
        assert_eq!(compiled.memory_alignment_checks(), 1);
        // The repeated store reuses its write proof. The load still needs an
        // independent read proof even though it uses the same exact address.
        assert_eq!(compiled.memory_permission_checks(), 2);
    }

    #[test]
    fn recognizes_affine_aliases_but_keeps_wrapping_page_boundaries_checked() {
        let machine = machine_with_code(&[addi(7, 6, 4), lw(5, 7, 0), lw(8, 6, 4)], IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert_eq!(compiled.memory_alignment_checks(), 1);
        assert_eq!(compiled.memory_permission_checks(), 1);

        let machine = machine_with_code(&[addi(6, 0, -4), lw(5, 6, 0), lw(7, 6, 4)], IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        // Both effective addresses are statically aligned, but RV32 wrapping
        // moves the second from the final padded page back to page zero.
        assert_eq!(compiled.memory_alignment_checks(), 0);
        assert_eq!(compiled.memory_permission_checks(), 2);
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
    fn emits_compact_immediates_and_omits_zero_address_adds() {
        let machine = machine_with_code(
            &[addi(5, 5, -128), addi(5, 5, 127), addi(5, 5, 128), NOP],
            IMAGE_START,
        );
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert!(contains_bytes(&compiled.code, &[0x41, 0x83, 0xc1, 0x80]));
        assert!(contains_bytes(&compiled.code, &[0x41, 0x83, 0xc1, 0x7f]));
        assert!(contains_bytes(
            &compiled.code,
            &[0x41, 0x81, 0xc1, 0x80, 0x00, 0x00, 0x00]
        ));

        let zero_machine = machine_with_code(&[lw(5, 6, 0), NOP], IMAGE_START);
        let offset_machine = machine_with_code(&[lw(5, 6, 4), NOP], IMAGE_START);
        let zero = compile(&decoded_block(&zero_machine, IMAGE_START)).unwrap();
        let offset = compile(&decoded_block(&offset_machine, IMAGE_START)).unwrap();
        assert_eq!(offset.code_len(), zero.code_len() + 3);
        assert!(!contains_bytes(&zero.code, &[0x83, 0xc0, 0x00]));
        assert!(contains_bytes(&offset.code, &[0x83, 0xc0, 0x04]));

        let zero_machine = machine_with_code(&[jalr(0, 6, 0)], IMAGE_START);
        let offset_machine = machine_with_code(&[jalr(0, 6, 4)], IMAGE_START);
        let zero = compile(&decoded_block(&zero_machine, IMAGE_START)).unwrap();
        let offset = compile(&decoded_block(&offset_machine, IMAGE_START)).unwrap();
        assert_eq!(offset.code_len(), zero.code_len() + 3);
        assert!(contains_bytes(&zero.code, &[0x83, 0xe0, 0xfe]));
    }

    #[test]
    fn emits_direct_branch_comparisons_for_every_operand_location() {
        let cached = |left: u32, right: u32| {
            let mut code = Vec::new();
            for register in [5, 6] {
                code.push(addi(register, register, 0));
                code.push(addi(register, register, 0));
            }
            code.push(branch(0, left, right, 8));
            let machine = machine_with_code(&code, IMAGE_START);
            compile(&decoded_block(&machine, IMAGE_START)).unwrap()
        };
        let canonical = |left: u32, right: u32| {
            let machine = machine_with_code(&[branch(0, left, right, 8)], IMAGE_START);
            compile(&decoded_block(&machine, IMAGE_START)).unwrap()
        };

        assert!(contains_bytes(
            &cached(5, 6).code,
            &[0x45, 0x39, 0xd1, 0x0f, 0x84]
        ));

        let mut host_canonical = vec![addi(5, 5, 0), addi(5, 5, 0)];
        host_canonical.push(branch(0, 5, 8, 8));
        let machine = machine_with_code(&host_canonical, IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert!(contains_bytes(
            &compiled.code,
            &[0x44, 0x3b, 0x4f, 32, 0x0f, 0x84]
        ));

        *host_canonical.last_mut().unwrap() = branch(0, 8, 5, 8);
        let machine = machine_with_code(&host_canonical, IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert!(contains_bytes(
            &compiled.code,
            &[0x44, 0x39, 0x4f, 32, 0x0f, 0x84]
        ));

        assert!(contains_bytes(
            &cached(5, 0).code,
            &[0x45, 0x85, 0xc9, 0x0f, 0x84]
        ));
        assert!(contains_bytes(
            &cached(0, 5).code,
            &[0x31, 0xc0, 0x41, 0x3b, 0xc1, 0x0f, 0x84]
        ));
        assert!(contains_bytes(
            &canonical(8, 0).code,
            &[0x83, 0x7f, 32, 0x00, 0x0f, 0x84]
        ));
        assert!(contains_bytes(
            &canonical(0, 8).code,
            &[0x31, 0xc0, 0x3b, 0x47, 32, 0x0f, 0x84]
        ));
        assert!(contains_bytes(
            &canonical(6, 7).code,
            &[0x8b, 0x47, 24, 0x3b, 0x47, 28, 0x0f, 0x84]
        ));
        assert!(contains_bytes(
            &canonical(0, 0).code,
            &[0x31, 0xc0, 0x0f, 0x84]
        ));
    }

    #[test]
    fn emits_direct_simple_alu_for_cached_and_canonical_operands() {
        let mut three_cached = Vec::new();
        for register in [5, 6, 7] {
            three_cached.push(addi(register, register, 0));
            three_cached.push(addi(register, register, 0));
        }
        three_cached.push(register(5, 6, 7, 0, 0));
        let machine = machine_with_code(&three_cached, IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert!(contains_bytes(
            &compiled.code,
            &[0x45, 0x89, 0xd1, 0x45, 0x01, 0xd9]
        ));

        *three_cached.last_mut().unwrap() = register(5, 6, 7, 0, 1);
        let machine = machine_with_code(&three_cached, IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert!(contains_bytes(
            &compiled.code,
            &[0x45, 0x89, 0xd1, 0x45, 0x0f, 0xaf, 0xcb]
        ));

        let code = [addi(5, 5, 0), addi(5, 5, 0), register(5, 5, 8, 0, 0)];
        let machine = machine_with_code(&code, IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert!(contains_bytes(&compiled.code, &[0x44, 0x03, 0x4f, 32]));

        let code = [
            addi(5, 5, 0),
            addi(5, 5, 0),
            addi(6, 6, 0),
            addi(6, 6, 0),
            register(8, 5, 6, 0, 0),
        ];
        let machine = machine_with_code(&code, IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert!(contains_bytes(
            &compiled.code,
            &[0x44, 0x89, 0xc8, 0x41, 0x03, 0xc2, 0x89, 0x47, 32]
        ));

        let code = [
            addi(5, 5, 0),
            addi(5, 5, 0),
            addi(6, 6, 0),
            addi(6, 6, 0),
            register(5, 6, 5, 0, 0x20),
        ];
        let machine = machine_with_code(&code, IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert!(contains_bytes(
            &compiled.code,
            &[0x44, 0x89, 0xd0, 0x41, 0x2b, 0xc1, 0x41, 0x89, 0xc1]
        ));
    }

    #[test]
    fn fuses_interleaved_complementary_shifts_without_renumbering_guest_work() {
        let code = [
            shift_immediate(6, 5, 1, 8),
            addi(9, 9, 1),
            shift_immediate(7, 5, 5, 24),
            register(8, 7, 6, 6, 0),
            addi(6, 0, 11),
            addi(7, 0, 12),
        ];
        let machine = machine_with_code(&code, IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();

        assert_eq!(compiled.fused_rotate_count(), 1);
        assert_eq!(compiled.elided_shift_count(), 2);
        assert_eq!(compiled.instruction_count(), code.len());
        assert_eq!(compiled.minimum_instruction_count(), code.len());
        assert!(contains_bytes(&compiled.code, &[0xc1, 0xc0, 8]));
        assert!(!contains_bytes(&compiled.code, &[0xc1, 0xe0, 8]));
        assert!(!contains_bytes(&compiled.code, &[0xc1, 0xe8, 24]));
    }

    #[test]
    fn rotates_in_either_direction_and_retains_live_shift_values() {
        let left = [
            shift_immediate(6, 5, 1, 7),
            shift_immediate(7, 5, 5, 25),
            register(6, 6, 7, 6, 0),
            addi(7, 0, 1),
        ];
        let right = [
            shift_immediate(6, 5, 1, 25),
            shift_immediate(7, 5, 5, 7),
            register(8, 6, 7, 6, 0),
            addi(6, 0, 1),
        ];

        let machine = machine_with_code(&left, IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert_eq!(compiled.fused_rotate_count(), 1);
        assert_eq!(compiled.elided_shift_count(), 2);
        assert!(contains_bytes(&compiled.code, &[0xc1, 0xc0, 7]));

        let machine = machine_with_code(&right, IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert_eq!(compiled.fused_rotate_count(), 1);
        assert_eq!(compiled.elided_shift_count(), 1);
        assert!(contains_bytes(&compiled.code, &[0xc1, 0xc8, 7]));
        assert!(contains_bytes(&compiled.code, &[0xc1, 0xe8, 7]));
    }

    #[test]
    fn rotate_fusion_respects_source_versions_and_side_exit_barriers() {
        let source_clobber = [
            shift_immediate(6, 5, 1, 8),
            addi(5, 5, 1),
            shift_immediate(7, 5, 5, 24),
            register(8, 6, 7, 6, 0),
        ];
        let memory_barrier = [
            shift_immediate(6, 5, 1, 8),
            lw(9, 10, 0),
            shift_immediate(7, 5, 5, 24),
            register(8, 6, 7, 6, 0),
        ];
        let division_barrier = [
            shift_immediate(6, 5, 1, 8),
            register(0, 10, 11, 4, 1),
            shift_immediate(7, 5, 5, 24),
            register(8, 6, 7, 6, 0),
        ];
        let source_temp_alias = [
            shift_immediate(6, 5, 5, 24),
            shift_immediate(5, 5, 1, 8),
            register(8, 6, 5, 6, 0),
        ];
        let fence_barrier = [
            shift_immediate(6, 5, 1, 8),
            0x0000_000f,
            shift_immediate(7, 5, 5, 24),
            register(8, 6, 7, 6, 0),
        ];
        let noncomplementary = [
            shift_immediate(6, 5, 1, 8),
            shift_immediate(7, 5, 5, 23),
            register(8, 6, 7, 6, 0),
        ];
        let masked_zero_count = [
            shift_immediate(6, 5, 1, 32),
            shift_immediate(7, 5, 5, 31),
            register(8, 6, 7, 6, 0),
        ];
        let arithmetic_right_shift = [
            shift_immediate(6, 5, 1, 8),
            shift_immediate(7, 5, 5, 24) | 0x4000_0000,
            register(8, 6, 7, 6, 0),
        ];
        let zero_destination = [
            shift_immediate(6, 5, 1, 8),
            shift_immediate(7, 5, 5, 24),
            register(0, 6, 7, 6, 0),
        ];
        let zero_temporary = [
            shift_immediate(0, 5, 1, 8),
            shift_immediate(7, 5, 5, 24),
            register(8, 0, 7, 6, 0),
        ];
        let both_producers_live = [
            shift_immediate(6, 5, 1, 8),
            shift_immediate(7, 5, 5, 24),
            register(8, 6, 7, 6, 0),
        ];

        for code in [
            source_clobber.as_slice(),
            memory_barrier.as_slice(),
            division_barrier.as_slice(),
            source_temp_alias.as_slice(),
            fence_barrier.as_slice(),
            noncomplementary.as_slice(),
            masked_zero_count.as_slice(),
            arithmetic_right_shift.as_slice(),
            zero_destination.as_slice(),
            zero_temporary.as_slice(),
            both_producers_live.as_slice(),
        ] {
            let machine = machine_with_code(code, IMAGE_START);
            let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
            assert_eq!(compiled.fused_rotate_count(), 0);
            assert_eq!(compiled.elided_shift_count(), 0);
        }
    }

    #[test]
    fn rotate_fusion_handles_x0_and_keeps_additionally_consumed_producers() {
        let zero_source = [
            shift_immediate(6, 0, 1, 8),
            shift_immediate(7, 0, 5, 24),
            register(8, 6, 7, 6, 0),
            addi(6, 0, 1),
            addi(7, 0, 2),
        ];
        let machine = machine_with_code(&zero_source, IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert_eq!(compiled.fused_rotate_count(), 1);
        assert_eq!(compiled.elided_shift_count(), 2);

        let extra_consumer = [
            shift_immediate(6, 5, 1, 8),
            addi(10, 6, 0),
            shift_immediate(7, 5, 5, 24),
            register(8, 6, 7, 6, 0),
            addi(6, 0, 1),
            addi(7, 0, 2),
        ];
        let machine = machine_with_code(&extra_consumer, IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert_eq!(compiled.fused_rotate_count(), 1);
        assert_eq!(compiled.elided_shift_count(), 1);
        assert!(contains_bytes(&compiled.code, &[0xc1, 0xe0, 8]));
    }

    #[test]
    fn emits_rotate_directly_into_cached_destinations() {
        let canonical_source = [
            addi(8, 8, 1),
            addi(8, 8, 2),
            shift_immediate(6, 5, 1, 8),
            shift_immediate(7, 5, 5, 24),
            register(8, 6, 7, 6, 0),
            addi(6, 0, 1),
            addi(7, 0, 2),
        ];
        let machine = machine_with_code(&canonical_source, IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert!(contains_bytes(
            &compiled.code,
            &[0x44, 0x8b, 0x4f, 20, 0x41, 0xc1, 0xc1, 8]
        ));

        let cached_source_and_destination = [
            addi(5, 5, 1),
            addi(5, 5, 2),
            shift_immediate(6, 5, 1, 8),
            shift_immediate(7, 5, 5, 24),
            register(5, 6, 7, 6, 0),
            addi(6, 0, 1),
            addi(7, 0, 2),
        ];
        let machine = machine_with_code(&cached_source_and_destination, IMAGE_START);
        let compiled = compile(&decoded_block(&machine, IMAGE_START)).unwrap();
        assert!(contains_bytes(&compiled.code, &[0x41, 0xc1, 0xc1, 8]));
    }

    #[test]
    fn control_transfer_splits_pure_rotate_spans() {
        let code = [
            shift_immediate(6, 5, 1, 8),
            branch(0, 0, 0, 4),
            shift_immediate(7, 5, 5, 24),
            register(8, 6, 7, 6, 0),
            addi(6, 0, 1),
            addi(7, 0, 2),
        ];
        let machine = machine_with_code(&code, IMAGE_START);
        let first = decoded(&machine, IMAGE_START, 2);
        let second = decoded(&machine, IMAGE_START + 8, 4);
        let compiled = compile_region(&[RegionBlock::new(&first), RegionBlock::new(&second)])
            .expect("branch target and fallthrough both continue to the second block");
        assert_eq!(compiled.fused_rotate_count(), 0);
        assert_eq!(compiled.elided_shift_count(), 0);
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
    fn padded_permission_guard_reduces_exits_without_changing_default_loop_policy() {
        let load_count = MAX_REGION_INSTRUCTIONS - 1;
        let mut code = vec![lw(5, 6, 0); load_count];
        code.push(jal(0, -508));
        let machine = machine_with_code(&code, IMAGE_START);
        let instructions = decoded(&machine, IMAGE_START, code.len());

        // Each checked word load now records only alignment and permission
        // exits: the padded permission table makes the latter the range guard.
        // Four explicit copies therefore remain inside the encoder's bounded
        // deferred-patch budget.
        assert!(load_count * 2 <= MAX_DEFERRED_EXIT_PATCHES);
        assert!(load_count * MAX_LOOP_GROUP_FACTOR * 2 <= MAX_DEFERRED_EXIT_PATCHES);

        let compiled = compile_loop(&[RegionBlock::new(&instructions)]).unwrap();
        assert_eq!(compiled.instruction_count(), MAX_REGION_INSTRUCTIONS);
        assert_eq!(
            compiled.minimum_instruction_count(),
            MAX_REGION_INSTRUCTIONS
        );
        assert_eq!(compiled.loop_unroll_factor(), 1);
        let grouped = compile_grouped_loop(&[RegionBlock::new(&instructions)], 4).unwrap();
        assert_eq!(grouped.loop_unroll_factor(), 4);
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
