//! VM5-private whole-image native linking.
//!
//! The shared block compiler deliberately keeps VM4's one-block call ABI.
//! VM5 reproduces that compiler's non-trapping lowering here so it can add a
//! per-block budget check and turn known exits into direct native edges.

use std::collections::BTreeMap;

use rv32vm_rust_common::{
    machine::DecodedInstruction,
    memory::{
        ADDRESS_SPACE_SIZE, DirectMemory, PAGE_COUNT, PAGE_SHIFT, PAGE_SIZE, PERM_READ, PERM_WRITE,
    },
};
use rv32vm_rust_x86_block_compiler::BlockInstruction;

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
const MAX_CACHED_REGISTERS: usize = 6;
const MIN_WEIGHTED_CACHE_ACCESSES: u64 = 5;
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
const INSTRUCTIONS_PER_PAGE: usize = PAGE_SIZE / size_of::<u32>();
pub(crate) const MAX_LINKED_BLOCKS: usize = 8_192;
const MAX_DISPATCH_BYTES: usize = PAGE_COUNT * size_of::<usize>()
    + MAX_LINKED_BLOCKS * (PAGE_SIZE + size_of::<Box<[u32; INSTRUCTIONS_PER_PAGE]>>());
#[cfg(feature = "profile")]
const PROFILE_BLOCKS_OFFSET: usize = 56;
#[cfg(feature = "profile")]
const PROFILE_DIRECT_LINKS_OFFSET: usize = 64;
#[cfg(feature = "profile")]
const PROFILE_INDIRECT_HITS_OFFSET: usize = 72;
#[cfg(feature = "profile")]
const PROFILE_INDIRECT_MISSES_OFFSET: usize = 80;
#[cfg(feature = "profile")]
const PROFILE_REGISTER_LOADS_OFFSET: usize = 88;
#[cfg(feature = "profile")]
const PROFILE_REGISTER_STORES_OFFSET: usize = 96;
#[cfg(feature = "profile")]
const PROFILE_CACHE_READ_HITS_OFFSET: usize = 104;
#[cfg(feature = "profile")]
const PROFILE_CACHE_WRITE_HITS_OFFSET: usize = 112;
#[cfg(feature = "profile")]
const PROFILE_FALLTHROUGH_OFFSET: usize = 120;
#[cfg(feature = "profile")]
const PROFILE_BRANCH_OFFSET: usize = 128;
#[cfg(feature = "profile")]
const PROFILE_JUMP_OFFSET: usize = 136;
#[cfg(feature = "profile")]
const PROFILE_MEMORY_LOADS_OFFSET: usize = 144;
#[cfg(feature = "profile")]
const PROFILE_MEMORY_STORES_OFFSET: usize = 152;
#[cfg(feature = "profile")]
const PROFILE_DIRECT_IMMEDIATE_OFFSET: usize = 160;
#[cfg(feature = "profile")]
const PROFILE_DIRECT_REGISTER_OFFSET: usize = 168;
#[cfg(feature = "profile")]
const PROFILE_DIRECT_BRANCH_OFFSET: usize = 176;
#[cfg(feature = "profile")]
const PROFILE_DIRECT_MEMORY_LOAD_OFFSET: usize = 184;
#[cfg(feature = "profile")]
const PROFILE_DIRECT_MEMORY_STORE_OFFSET: usize = 192;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockFlow {
    Fallthrough {
        pc: u32,
    },
    Branch {
        condition: Condition,
        fallthrough: u32,
        target: u32,
    },
    Jump {
        pc: u32,
    },
    IndirectJump {
        target_hint: Option<u32>,
    },
}

impl BlockFlow {
    const fn successors(self) -> [Option<u32>; 2] {
        match self {
            Self::Fallthrough { pc } | Self::Jump { pc } => [Some(pc), None],
            Self::Branch {
                fallthrough,
                target,
                ..
            } => [Some(fallthrough), Some(target)],
            Self::IndirectJump { target_hint } => [target_hint, None],
        }
    }
}

/// A bounded native block staged during LOAD, before image-wide relocation.
pub(crate) struct LinkedBlock {
    pc: u32,
    instructions: Vec<Lowering>,
    flow: BlockFlow,
    reserved_code_len: usize,
}

impl LinkedBlock {
    /// Reports whether VM5's private linked backend can lower an instruction.
    ///
    /// AOT discovery uses this predicate so candidate boundaries cannot drift
    /// when the separately versioned VM4 block compiler gains a lowering.
    pub(crate) fn supports(instruction: DecodedInstruction) -> bool {
        Lowering::decode(instruction).is_some()
    }

    /// Reports whether this supported instruction terminates a native region.
    pub(crate) fn ends_block(instruction: DecodedInstruction) -> bool {
        matches!(
            Lowering::decode(instruction),
            Some(Lowering::Jump { .. } | Lowering::IndirectJump { .. } | Lowering::Branch { .. })
        )
    }

    /// Reports whether an instruction needs a separately compiled successor
    /// for resuming after its precise one-instruction slow path.
    pub(crate) fn needs_precise_resume(instruction: DecodedInstruction) -> bool {
        matches!(
            Lowering::decode(instruction),
            Some(Lowering::Load { .. } | Lowering::Store { .. })
        )
    }

    pub(crate) fn compile(instructions: &[BlockInstruction]) -> Option<Self> {
        let first = *instructions.first()?.as_ref().ok()?;
        let pc = first.pc();
        let mut next_pc = pc;
        let mut lowered = Vec::new();
        let mut flow = None;

        for instruction in instructions {
            let Ok(instruction) = *instruction else {
                break;
            };
            if instruction.pc() != next_pc {
                break;
            }
            let Some(lowering) = Lowering::decode(instruction) else {
                break;
            };
            next_pc = next_pc.wrapping_add(4);
            let terminal = match lowering {
                Lowering::IndirectJump {
                    source, immediate, ..
                } => Some(BlockFlow::IndirectJump {
                    target_hint: indirect_target_hint(&lowered, source, immediate),
                }),
                Lowering::Load { .. } | Lowering::Store { .. } => None,
                _ => lowering.flow(next_pc),
            };
            lowered.push(lowering);
            if terminal.is_some() {
                flow = terminal;
                break;
            }
        }

        if lowered.is_empty() {
            return None;
        }
        let flow = flow.unwrap_or(BlockFlow::Fallthrough { pc: next_pc });
        let reserved_code_len = reserved_len(&lowered, flow)?;
        Some(Self {
            pc,
            instructions: lowered,
            flow,
            reserved_code_len,
        })
    }

    #[cfg(feature = "profile")]
    pub(crate) fn instruction_count(&self) -> usize {
        self.instructions.len()
    }

    /// Conservative admission charge for this block. Every outgoing edge
    /// reserves a cold missing-successor veneer even though resolved edges omit
    /// it from the finalized image.
    pub(crate) const fn reserved_code_len(&self) -> usize {
        self.reserved_code_len
    }

    pub(crate) fn successors(&self) -> Vec<u32> {
        let mut successors = self
            .instructions
            .iter()
            .filter_map(|instruction| match instruction {
                Lowering::Load { pc, .. } | Lowering::Store { pc, .. } => Some(pc.wrapping_add(4)),
                _ => None,
            })
            .collect::<Vec<_>>();
        successors.extend(self.flow.successors().into_iter().flatten());
        successors.sort_unstable();
        successors.dedup();
        successors
    }

    pub(crate) const fn flow_successors(&self) -> [Option<u32>; 2] {
        self.flow.successors()
    }

    #[cfg(feature = "profile")]
    pub(crate) const fn flow(&self) -> BlockFlow {
        self.flow
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lowering {
    WriteImmediate {
        destination: usize,
        value: u32,
    },
    Jump {
        destination: usize,
        link: u32,
        target: u32,
    },
    IndirectJump {
        pc: u32,
        destination: usize,
        source: usize,
        immediate: u32,
        link: u32,
    },
    Branch {
        left: usize,
        right: usize,
        condition: Condition,
        fallthrough: u32,
        target: u32,
    },
    Immediate {
        destination: usize,
        source: usize,
        operation: ImmediateOperation,
    },
    Register {
        destination: usize,
        left: usize,
        right: usize,
        operation: RegisterOperation,
    },
    Load {
        pc: u32,
        destination: usize,
        base: usize,
        immediate: u32,
        width: MemoryWidth,
        signed: bool,
    },
    Store {
        pc: u32,
        base: usize,
        source: usize,
        immediate: u32,
        width: MemoryWidth,
    },
    Fence,
}

impl Lowering {
    fn decode(instruction: DecodedInstruction) -> Option<Self> {
        match instruction.opcode() {
            0x37 => Some(Self::WriteImmediate {
                destination: instruction.rd(),
                value: instruction.raw() & 0xffff_f000,
            }),
            0x17 => Some(Self::WriteImmediate {
                destination: instruction.rd(),
                value: instruction
                    .pc()
                    .wrapping_add(instruction.raw() & 0xffff_f000),
            }),
            0x6f => {
                let target = instruction.jump_target();
                target.is_multiple_of(4).then_some(Self::Jump {
                    destination: instruction.rd(),
                    link: instruction.pc().wrapping_add(4),
                    target,
                })
            }
            0x67 if instruction.funct3() == 0 => Some(Self::IndirectJump {
                pc: instruction.pc(),
                destination: instruction.rd(),
                source: instruction.rs1(),
                immediate: sign_extend(instruction.raw() >> 20, 12),
                link: instruction.pc().wrapping_add(4),
            }),
            0x63 => {
                let condition = match instruction.funct3() {
                    0 => Condition::Equal,
                    1 => Condition::NotEqual,
                    4 => Condition::LessThan,
                    5 => Condition::GreaterOrEqual,
                    6 => Condition::Below,
                    7 => Condition::AboveOrEqual,
                    _ => return None,
                };
                let target = instruction.branch_target();
                target.is_multiple_of(4).then_some(Self::Branch {
                    left: instruction.rs1(),
                    right: instruction.rs2(),
                    condition,
                    fallthrough: instruction.pc().wrapping_add(4),
                    target,
                })
            }
            0x13 => Some(Self::Immediate {
                destination: instruction.rd(),
                source: instruction.rs1(),
                operation: ImmediateOperation::decode(instruction)?,
            }),
            0x33 => Some(Self::Register {
                destination: instruction.rd(),
                left: instruction.rs1(),
                right: instruction.rs2(),
                operation: RegisterOperation::decode(instruction)?,
            }),
            0x03 => {
                let (width, signed) = match instruction.funct3() {
                    0 => (MemoryWidth::Byte, true),
                    1 => (MemoryWidth::Half, true),
                    2 => (MemoryWidth::Word, false),
                    4 => (MemoryWidth::Byte, false),
                    5 => (MemoryWidth::Half, false),
                    _ => return None,
                };
                Some(Self::Load {
                    pc: instruction.pc(),
                    destination: instruction.rd(),
                    base: instruction.rs1(),
                    immediate: sign_extend(instruction.raw() >> 20, 12),
                    width,
                    signed,
                })
            }
            0x23 => {
                let width = match instruction.funct3() {
                    0 => MemoryWidth::Byte,
                    1 => MemoryWidth::Half,
                    2 => MemoryWidth::Word,
                    _ => return None,
                };
                let encoded =
                    ((instruction.raw() >> 7) & 0x1f) | (((instruction.raw() >> 25) & 0x7f) << 5);
                Some(Self::Store {
                    pc: instruction.pc(),
                    base: instruction.rs1(),
                    source: instruction.rs2(),
                    immediate: sign_extend(encoded, 12),
                    width,
                })
            }
            0x0f if instruction.funct3() == 0 => Some(Self::Fence),
            _ => None,
        }
    }

    const fn flow(self, next_pc: u32) -> Option<BlockFlow> {
        match self {
            Self::Jump { target, .. } => Some(BlockFlow::Jump { pc: target }),
            Self::Branch {
                condition,
                fallthrough,
                target,
                ..
            } => Some(BlockFlow::Branch {
                condition,
                fallthrough,
                target,
            }),
            Self::IndirectJump { .. } => Some(BlockFlow::IndirectJump { target_hint: None }),
            Self::Load { .. } | Self::Store { .. } => None,
            _ => {
                let _ = next_pc;
                None
            }
        }
    }

    fn score_register_uses(
        self,
        scores: &mut [u64; 32],
        weighted_accesses: &mut [u64; 32],
        execution_weight: u64,
    ) {
        fn add(array: &mut [u64; 32], register: usize, weight: u64) {
            if register != 0 {
                array[register] = array[register].saturating_add(weight);
            }
        }
        fn read(
            scores: &mut [u64; 32],
            weighted_accesses: &mut [u64; 32],
            register: usize,
            execution_weight: u64,
        ) {
            add(scores, register, execution_weight.saturating_mul(2));
            add(weighted_accesses, register, execution_weight);
        }
        fn write(
            scores: &mut [u64; 32],
            weighted_accesses: &mut [u64; 32],
            register: usize,
            execution_weight: u64,
        ) {
            add(scores, register, execution_weight);
            add(weighted_accesses, register, execution_weight);
        }
        match self {
            Self::WriteImmediate { destination, .. } | Self::Jump { destination, .. } => {
                write(scores, weighted_accesses, destination, execution_weight);
            }
            Self::IndirectJump {
                destination,
                source,
                ..
            } => {
                read(scores, weighted_accesses, source, execution_weight);
                write(scores, weighted_accesses, destination, execution_weight);
            }
            Self::Branch { left, right, .. } => {
                read(scores, weighted_accesses, left, execution_weight);
                read(scores, weighted_accesses, right, execution_weight);
            }
            Self::Immediate {
                destination,
                source,
                ..
            } => {
                // The native lowering deliberately elides an instruction
                // whose destination is x0 and which cannot trap.
                if destination != 0 {
                    read(scores, weighted_accesses, source, execution_weight);
                    write(scores, weighted_accesses, destination, execution_weight);
                }
            }
            Self::Register {
                destination,
                left,
                right,
                ..
            } => {
                if destination != 0 {
                    read(scores, weighted_accesses, left, execution_weight);
                    read(scores, weighted_accesses, right, execution_weight);
                    write(scores, weighted_accesses, destination, execution_weight);
                }
            }
            Self::Load {
                destination, base, ..
            } => {
                read(scores, weighted_accesses, base, execution_weight);
                write(scores, weighted_accesses, destination, execution_weight);
            }
            Self::Store { base, source, .. } => {
                read(scores, weighted_accesses, base, execution_weight);
                read(scores, weighted_accesses, source, execution_weight);
            }
            Self::Fence => {}
        }
    }

    #[cfg(not(feature = "profile"))]
    const fn writes_register(self, register: usize) -> bool {
        if register == 0 {
            return false;
        }
        match self {
            Self::WriteImmediate { destination, .. }
            | Self::Jump { destination, .. }
            | Self::IndirectJump { destination, .. }
            | Self::Immediate { destination, .. }
            | Self::Register { destination, .. }
            | Self::Load { destination, .. } => destination == register,
            Self::Branch { .. } | Self::Store { .. } | Self::Fence => false,
        }
    }
}

fn indirect_target_hint(preceding: &[Lowering], source: usize, immediate: u32) -> Option<u32> {
    let base = if source == 0 {
        0
    } else {
        let Lowering::WriteImmediate { destination, value } = *preceding.last()? else {
            return None;
        };
        (destination == source).then_some(value)?
    };
    let target = base.wrapping_add(immediate) & !1;
    target.is_multiple_of(4).then_some(target)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MemoryWidth {
    Byte,
    Half,
    Word,
}

impl MemoryWidth {
    const fn bytes(self) -> u32 {
        match self {
            Self::Byte => 1,
            Self::Half => 2,
            Self::Word => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Condition {
    Equal,
    NotEqual,
    LessThan,
    GreaterOrEqual,
    Below,
    AboveOrEqual,
}

impl Condition {
    const fn x86(self) -> u8 {
        match self {
            Self::Equal => 0x84,
            Self::NotEqual => 0x85,
            Self::LessThan => 0x8c,
            Self::GreaterOrEqual => 0x8d,
            Self::Below => 0x82,
            Self::AboveOrEqual => 0x83,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImmediateOperation {
    Add(u32),
    SetLessThan(u32),
    SetBelow(u32),
    Xor(u32),
    Or(u32),
    And(u32),
    ShiftLeft(u8),
    ShiftRight(u8),
    ShiftRightArithmetic(u8),
}

impl ImmediateOperation {
    fn decode(instruction: DecodedInstruction) -> Option<Self> {
        let immediate = || sign_extend(instruction.raw() >> 20, 12);
        match (instruction.funct3(), instruction.funct7()) {
            (0, _) => Some(Self::Add(immediate())),
            (2, _) => Some(Self::SetLessThan(immediate())),
            (3, _) => Some(Self::SetBelow(immediate())),
            (4, _) => Some(Self::Xor(immediate())),
            (6, _) => Some(Self::Or(immediate())),
            (7, _) => Some(Self::And(immediate())),
            (1, 0) => Some(Self::ShiftLeft(instruction.rs2() as u8)),
            (5, 0) => Some(Self::ShiftRight(instruction.rs2() as u8)),
            (5, 0x20) => Some(Self::ShiftRightArithmetic(instruction.rs2() as u8)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegisterOperation {
    Add,
    Subtract,
    ShiftLeft,
    SetLessThan,
    SetBelow,
    Xor,
    ShiftRight,
    ShiftRightArithmetic,
    Or,
    And,
    Multiply,
    MultiplyHighSigned,
    MultiplyHighSignedUnsigned,
    MultiplyHighUnsigned,
    DivideSigned,
    DivideUnsigned,
    RemainderSigned,
    RemainderUnsigned,
}

impl RegisterOperation {
    fn decode(instruction: DecodedInstruction) -> Option<Self> {
        match (instruction.funct7(), instruction.funct3()) {
            (0, 0) => Some(Self::Add),
            (0x20, 0) => Some(Self::Subtract),
            (0, 1) => Some(Self::ShiftLeft),
            (0, 2) => Some(Self::SetLessThan),
            (0, 3) => Some(Self::SetBelow),
            (0, 4) => Some(Self::Xor),
            (0, 5) => Some(Self::ShiftRight),
            (0x20, 5) => Some(Self::ShiftRightArithmetic),
            (0, 6) => Some(Self::Or),
            (0, 7) => Some(Self::And),
            (1, 0) => Some(Self::Multiply),
            (1, 1) => Some(Self::MultiplyHighSigned),
            (1, 2) => Some(Self::MultiplyHighSignedUnsigned),
            (1, 3) => Some(Self::MultiplyHighUnsigned),
            (1, 4) => Some(Self::DivideSigned),
            (1, 5) => Some(Self::DivideUnsigned),
            (1, 6) => Some(Self::RemainderSigned),
            (1, 7) => Some(Self::RemainderUnsigned),
            _ => None,
        }
    }
}

const fn sign_extend(value: u32, bits: u32) -> u32 {
    ((value << (32 - bits)) as i32 >> (32 - bits)) as u32
}

fn reserved_len(instructions: &[Lowering], flow: BlockFlow) -> Option<usize> {
    // An uncached body is a mapping-independent upper bound: every cached
    // register move is no larger than the corresponding [RSI+disp8] access.
    let mut emitter = Emitter::new(RegisterCache::empty());
    emitter.emit_block(instructions, flow, 0)?;
    emitter.reserved_code_len()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum CachedHost {
    Ebx,
    Ebp,
    R12d,
    R13d,
    R14d,
    R15d,
}

impl CachedHost {
    const ALL: [Self; MAX_CACHED_REGISTERS] = [
        Self::Ebx,
        Self::Ebp,
        Self::R12d,
        Self::R13d,
        Self::R14d,
        Self::R15d,
    ];

    const fn register(self) -> Register32 {
        match self {
            Self::Ebx => Register32::Ebx,
            Self::Ebp => Register32::Ebp,
            Self::R12d => Register32::R12d,
            Self::R13d => Register32::R13d,
            Self::R14d => Register32::R14d,
            Self::R15d => Register32::R15d,
        }
    }
}

/// An x86-64 general-purpose register viewed through its wrapping 32-bit
/// subregister. Writing one of these always zero-extends into the full host
/// register, exactly matching RV32 arithmetic modulo 2^32.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum Register32 {
    Eax = 0,
    Ecx = 1,
    Ebx = 3,
    Ebp = 5,
    R12d = 12,
    R13d = 13,
    R14d = 14,
    R15d = 15,
    #[cfg(not(feature = "profile"))]
    R11d = 11,
}

impl Register32 {
    const fn encoding(self) -> u8 {
        self as u8
    }
}

/// One encodable x86 r/m32 operand used by the linked backend. Guest memory is
/// always `[RSI + disp8]`; every nonzero RV32 register offset fits exactly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Operand32 {
    Register(Register32),
    GuestMemory(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BinaryOperation32 {
    Add,
    Subtract,
    Xor,
    Or,
    And,
    Multiply,
}

impl BinaryOperation32 {
    const fn opcode(self) -> &'static [u8] {
        match self {
            Self::Add => &[0x03],
            Self::Subtract => &[0x2b],
            Self::Xor => &[0x33],
            Self::Or => &[0x0b],
            Self::And => &[0x23],
            Self::Multiply => &[0x0f, 0xaf],
        }
    }

    const fn commutative(self) -> bool {
        !matches!(self, Self::Subtract)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RegisterCache {
    guests: [u8; MAX_CACHED_REGISTERS],
    count: u8,
    host_by_guest: [u8; 32],
}

impl RegisterCache {
    const NONE: u8 = u8::MAX;

    const fn empty() -> Self {
        Self {
            guests: [0; MAX_CACHED_REGISTERS],
            count: 0,
            host_by_guest: [Self::NONE; 32],
        }
    }

    fn scores_and_weighted_accesses(blocks: &[LinkedBlock]) -> ([u64; 32], [u64; 32]) {
        // Overlapping eager candidates often contain the same guest word, so
        // score each instruction address exactly once. Backward conditional
        // edges and tail jumps identify bounded natural-loop intervals; each
        // enclosing loop adds the same generic 8x hotness proxy. This remains
        // entirely image-derived and deterministic, without runtime profiles.
        let instruction_capacity = blocks
            .iter()
            .fold(0_usize, |total, block| {
                total.saturating_add(block.instructions.len())
            })
            .min(MAX_LINKED_BLOCKS * 64);
        let mut instructions = Vec::<(u32, Lowering)>::with_capacity(instruction_capacity);
        for block in blocks {
            for (index, &instruction) in block.instructions.iter().enumerate() {
                let Some(offset) = u32::try_from(index)
                    .ok()
                    .and_then(|index| index.checked_mul(4))
                else {
                    continue;
                };
                if let Some(pc) = block.pc.checked_add(offset) {
                    instructions.push((pc, instruction));
                }
            }
        }
        instructions.sort_unstable_by_key(|(pc, _)| *pc);
        instructions.dedup_by(|right, left| {
            if right.0 != left.0 {
                return false;
            }
            debug_assert_eq!(right.1, left.1);
            true
        });

        let mut loop_intervals = Vec::with_capacity(blocks.len());
        for &(pc, instruction) in &instructions {
            let target = match instruction {
                Lowering::Branch { target, .. } if target <= pc => Some(target),
                Lowering::Jump {
                    destination: 0,
                    target,
                    ..
                } if target <= pc => Some(target),
                _ => None,
            };
            if let Some(target) = target {
                loop_intervals.push((target, pc));
            }
        }
        loop_intervals.sort_unstable();
        loop_intervals.dedup();
        debug_assert!(loop_intervals.len() <= blocks.len());
        let mut loop_events = Vec::with_capacity(loop_intervals.len().saturating_mul(2));
        for (start, end) in loop_intervals {
            loop_events.push((u64::from(start), 1_i64));
            loop_events.push((u64::from(end) + 4, -1_i64));
        }
        loop_events.sort_unstable_by_key(|(pc, _)| *pc);

        let mut scores = [0_u64; 32];
        let mut weighted_accesses = [0_u64; 32];
        let mut event_index = 0;
        let mut loop_depth = 0_i64;
        for &(pc, instruction) in &instructions {
            while event_index < loop_events.len() && loop_events[event_index].0 <= u64::from(pc) {
                loop_depth += loop_events[event_index].1;
                event_index += 1;
            }
            debug_assert!(loop_depth >= 0);
            let weight = 1_u64.saturating_add((loop_depth as u64).saturating_mul(7));
            instruction.score_register_uses(&mut scores, &mut weighted_accesses, weight);
        }
        (scores, weighted_accesses)
    }

    #[cfg(test)]
    fn scores(blocks: &[LinkedBlock]) -> [u64; 32] {
        Self::scores_and_weighted_accesses(blocks).0
    }

    fn select(blocks: &[LinkedBlock]) -> Self {
        let (scores, weighted_accesses) = Self::scores_and_weighted_accesses(blocks);

        let mut ranked = (1_u8..32).collect::<Vec<_>>();
        ranked.sort_unstable_by(|left, right| {
            scores[*right as usize]
                .cmp(&scores[*left as usize])
                .then_with(|| left.cmp(right))
        });

        let mut cache = Self::empty();
        for guest in ranked
            .into_iter()
            .filter(|guest| weighted_accesses[*guest as usize] >= MIN_WEIGHTED_CACHE_ACCESSES)
            .take(MAX_CACHED_REGISTERS)
        {
            let host = usize::from(cache.count);
            cache.guests[host] = guest;
            cache.host_by_guest[guest as usize] = host as u8;
            cache.count += 1;
        }
        cache
    }

    fn host(self, guest: usize) -> Option<CachedHost> {
        let index = *self.host_by_guest.get(guest)?;
        (index != Self::NONE).then(|| CachedHost::ALL[index as usize])
    }

    const fn is_empty(self) -> bool {
        self.count == 0
    }

    fn entries(self) -> impl DoubleEndedIterator<Item = (CachedHost, usize)> {
        CachedHost::ALL
            .into_iter()
            .zip(self.guests.map(usize::from))
            .take(usize::from(self.count))
    }

    #[cfg(any(test, feature = "profile"))]
    const fn count(self) -> usize {
        self.count as usize
    }

    #[cfg(any(test, feature = "profile"))]
    const fn guests(self) -> [u8; MAX_CACHED_REGISTERS] {
        self.guests
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
            self.code.extend_from_slice(&[0x48, 0x8b, 0x37]); // mov rsi, [rdi]
            self.code.extend_from_slice(&[0x4c, 0x8b, 0x47, 0x18]); // mov r8, [rdi+24]
            self.code.extend_from_slice(&[0x4c, 0x8b, 0x4f, 0x20]); // mov r9, [rdi+32]
            self.code.extend_from_slice(&[0x4c, 0x8b, 0x57, 0x08]); // mov r10, [rdi+8]
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
                    self.checked_load(pc, refund, destination, base, immediate, width, signed)?;
                }
                Lowering::Store {
                    pc,
                    base,
                    source,
                    immediate,
                    width,
                } => {
                    self.checked_store(pc, refund, base, source, immediate, width)?;
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
        self.code.extend_from_slice(&[0x48, 0x8b, 0x37]); // mov rsi, [rdi]
        self.code.extend_from_slice(&[0x4c, 0x8b, 0x47, 0x18]); // mov r8, [rdi+24]
        self.code.extend_from_slice(&[0x4c, 0x8b, 0x4f, 0x20]); // mov r9, [rdi+32]
        self.code.extend_from_slice(&[0x4c, 0x8b, 0x57, 0x08]); // mov r10, [rdi+8]
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
            self.conditional_edge(condition.x86(), target)?;
            self.edge_slot(fallthrough)
        }
        #[cfg(feature = "profile")]
        {
            self.code.extend_from_slice(&[0x0f, condition.x86()]);
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
        self.code.extend_from_slice(&[0x4c, 0x8b, 0x5f, 0x28]); // mov r11, [rdi+40]
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
        self.code.extend_from_slice(&[0x4c, 0x8b, 0x5f, 0x30]); // mov r11, [rdi+48]
        self.code.extend_from_slice(&[0x49, 0x01, 0xc3]); // add r11, rax
        #[cfg(feature = "profile")]
        self.increment_context(PROFILE_INDIRECT_HITS_OFFSET);
        self.code.extend_from_slice(&[0x41, 0xff, 0xe3]); // jmp r11

        self.indirect_misses.extend(misses);
        self.interpret_one_exit(vec![misaligned], pc, 1)
    }

    fn checked_load(
        &mut self,
        pc: u32,
        refund: u8,
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
        self.interpret_one_exit(failures, pc, refund)
    }

    fn checked_store(
        &mut self,
        pc: u32,
        refund: u8,
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
        self.interpret_one_exit(failures, pc, refund)
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
        self.code.extend_from_slice(&[0x4c, 0x89, 0x57, 0x08]); // mov [rdi+8], r10
        self.code.extend_from_slice(&[0x89, 0x47, 0x10]); // mov [rdi+16], eax
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
        self.code.extend_from_slice(&[0xc7, 0x47, 0x14]);
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

/// Sparse immutable guest-PC to native-entry offsets used only by in-image
/// indirect dispatch. Leaves store offset-plus-one so zero remains a miss.
struct DispatchTable {
    roots: Box<[usize]>,
    _leaves: Vec<Box<[u32; INSTRUCTIONS_PER_PAGE]>>,
    _entries: usize,
    _bytes: usize,
}

impl DispatchTable {
    fn build(code: &[u8], entries: &[(u32, EntryMetadata)]) -> Option<Self> {
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

    const fn roots_ptr(&self) -> *const usize {
        self.roots.as_ptr()
    }

    #[cfg(any(test, feature = "profile"))]
    const fn page_count(&self) -> usize {
        self._leaves.len()
    }

    #[cfg(any(test, feature = "profile"))]
    const fn entry_count(&self) -> usize {
        self._entries
    }

    #[cfg(any(test, feature = "profile"))]
    const fn bytes(&self) -> usize {
        self._bytes
    }

    #[cfg(test)]
    fn encoded_entry(&self, pc: u32) -> Option<u32> {
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

#[repr(C)]
struct RunContext {
    registers: *mut u32,
    remaining: u64,
    pc: u32,
    exit: u32,
    permissions: *const u8,
    address_space: *mut u8,
    dispatch_pages: *const usize,
    code_base: *const u8,
    #[cfg(feature = "profile")]
    blocks: u64,
    #[cfg(feature = "profile")]
    direct_links: u64,
    #[cfg(feature = "profile")]
    indirect_hits: u64,
    #[cfg(feature = "profile")]
    indirect_misses: u64,
    #[cfg(feature = "profile")]
    register_loads: u64,
    #[cfg(feature = "profile")]
    register_stores: u64,
    #[cfg(feature = "profile")]
    cache_read_hits: u64,
    #[cfg(feature = "profile")]
    cache_write_hits: u64,
    #[cfg(feature = "profile")]
    fallthrough_blocks: u64,
    #[cfg(feature = "profile")]
    branch_blocks: u64,
    #[cfg(feature = "profile")]
    jump_blocks: u64,
    #[cfg(feature = "profile")]
    memory_loads: u64,
    #[cfg(feature = "profile")]
    memory_stores: u64,
    #[cfg(feature = "profile")]
    direct_immediate: u64,
    #[cfg(feature = "profile")]
    direct_register: u64,
    #[cfg(feature = "profile")]
    direct_branch: u64,
    #[cfg(feature = "profile")]
    direct_memory_load: u64,
    #[cfg(feature = "profile")]
    direct_memory_store: u64,
}

impl RunContext {
    fn new(
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

const _: () = assert!(std::mem::offset_of!(RunContext, registers) == 0);
const _: () = assert!(std::mem::offset_of!(RunContext, remaining) == 8);
const _: () = assert!(std::mem::offset_of!(RunContext, pc) == 16);
const _: () = assert!(std::mem::offset_of!(RunContext, exit) == 20);
const _: () = assert!(std::mem::offset_of!(RunContext, permissions) == 24);
const _: () = assert!(std::mem::offset_of!(RunContext, address_space) == 32);
const _: () = assert!(std::mem::offset_of!(RunContext, dispatch_pages) == 40);
const _: () = assert!(std::mem::offset_of!(RunContext, code_base) == 48);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, blocks) == 56);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, direct_links) == 64);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, indirect_hits) == 72);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, indirect_misses) == 80);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, register_loads) == 88);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, register_stores) == 96);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, cache_read_hits) == 104);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, cache_write_hits) == 112);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, fallthrough_blocks) == 120);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, branch_blocks) == 128);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, jump_blocks) == 136);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, memory_loads) == 144);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, memory_stores) == 152);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, direct_immediate) == 160);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, direct_register) == 168);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, direct_branch) == 176);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, direct_memory_load) == 184);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, direct_memory_store) == 192);

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
    memory: ExecutableMemory,
    entries: Vec<EntryMetadata>,
    dispatch: DispatchTable,
    #[cfg(any(test, feature = "profile"))]
    cache: RegisterCache,
    #[cfg(feature = "profile")]
    hot_code_bytes: usize,
    #[cfg(feature = "profile")]
    cold_code_bytes: usize,
    #[cfg(feature = "profile")]
    external_thunk_bytes: usize,
    #[cfg(feature = "profile")]
    shared_prologue_bytes: usize,
    #[cfg(feature = "profile")]
    exit_trampoline_bytes: usize,
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

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
struct ExecutableMemory {
    address: std::ptr::NonNull<u8>,
    length: usize,
    #[cfg(test)]
    unmap_status: std::sync::Arc<std::sync::atomic::AtomicI32>,
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
impl ExecutableMemory {
    fn publish(code: &[u8], byte_budget: usize) -> Option<Self> {
        use std::{ffi::c_void, ptr};

        const PROT_READ: i32 = 0x1;
        const PROT_WRITE: i32 = 0x2;
        const PROT_EXEC: i32 = 0x4;
        const MAP_PRIVATE: i32 = 0x2;
        const MAP_ANONYMOUS: i32 = 0x20;
        unsafe extern "C" {
            fn getpagesize() -> i32;
            fn mmap(
                address: *mut c_void,
                length: usize,
                protection: i32,
                flags: i32,
                file: i32,
                offset: i64,
            ) -> *mut c_void;
            fn mprotect(address: *mut c_void, length: usize, protection: i32) -> i32;
            fn munmap(address: *mut c_void, length: usize) -> i32;
        }

        // SAFETY: `getpagesize` has no arguments and returns process metadata.
        let page_size = usize::try_from(unsafe { getpagesize() }).ok()?;
        let length = mapping_length(code.len(), page_size, byte_budget)?;
        // SAFETY: A fresh private anonymous mapping is checked before use.
        let raw = unsafe {
            mmap(
                ptr::null_mut(),
                length,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if raw as isize == -1 {
            return None;
        }
        let Some(address) = std::ptr::NonNull::new(raw.cast::<u8>()) else {
            // SAFETY: `raw` is the complete successful mapping.
            unsafe { munmap(raw, length) };
            return None;
        };
        // SAFETY: The mapping is uniquely owned, writable, and large enough.
        unsafe { ptr::copy_nonoverlapping(code.as_ptr(), address.as_ptr(), code.len()) };
        // SAFETY: This covers exactly the owned mapping and removes write access.
        if unsafe { mprotect(raw, length, PROT_READ | PROT_EXEC) } != 0 {
            // SAFETY: The mapping is still owned here after failed publication.
            unsafe { munmap(raw, length) };
            return None;
        }
        Some(Self {
            address,
            length,
            #[cfg(test)]
            unmap_status: std::sync::Arc::new(std::sync::atomic::AtomicI32::new(i32::MIN)),
        })
    }

    const fn address(&self) -> *const u8 {
        self.address.as_ptr()
    }

    const fn len(&self) -> usize {
        self.length
    }

    #[cfg(test)]
    fn unmap_status(&self) -> std::sync::Arc<std::sync::atomic::AtomicI32> {
        std::sync::Arc::clone(&self.unmap_status)
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
impl Drop for ExecutableMemory {
    fn drop(&mut self) {
        unsafe extern "C" {
            fn munmap(address: *mut std::ffi::c_void, length: usize) -> i32;
        }
        // SAFETY: This owner holds the complete live mapping exactly once.
        let _status = unsafe { munmap(self.address.as_ptr().cast(), self.length) };
        #[cfg(test)]
        self.unmap_status
            .store(_status, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
)))]
struct ExecutableMemory;

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
)))]
impl ExecutableMemory {
    fn publish(_code: &[u8], _byte_budget: usize) -> Option<Self> {
        None
    }

    const fn len(&self) -> usize {
        0
    }
}

fn mapping_length(code_len: usize, page_size: usize, byte_budget: usize) -> Option<usize> {
    if code_len == 0 || page_size == 0 {
        return None;
    }
    let pages = code_len.checked_add(page_size - 1)? / page_size;
    let length = pages.checked_mul(page_size)?;
    (length <= byte_budget).then_some(length)
}

#[cfg(test)]
mod tests {
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use rv32vm_rust_common::memory::{PAGE_SHIFT, PERM_READ, PERM_WRITE, STACK_START};
    use rv32vm_rust_common::{machine::Machine, memory::IMAGE_START};
    use rv32vm_rust_x86_block_compiler::BlockInstruction;

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use super::NativeStop;
    use super::{
        BUDGET_VENEER_BYTES, BinaryOperation32, DispatchTable, EDGE_SLOT_BYTES, ENTRY_BYTES,
        EXIT_BUDGET, EXIT_INTERPRET_ONE, EXIT_MISSING, EXTERNAL_THUNK_BYTES, Emitter,
        INTERPRET_ONE_VENEER_BYTES, LinkedBlock, LinkedProgram, MAX_EXIT_TRAMPOLINE_BYTES,
        MAX_FIXED_CODE_BYTES, MAX_SHARED_PROLOGUE_BYTES, MIN_WEIGHTED_CACHE_ACCESSES,
        MISSING_VENEER_BYTES, MemoryWidth, Operand32, Register32, RegisterCache, mapping_length,
    };
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use crate::test_support::beq;
    use crate::test_support::{addi, image_with_code_at, jal, jalr, lw};

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    core::arch::global_asm!(
        r#"
        .text
        .globl vm5_cache6_abi_probe
        .type vm5_cache6_abi_probe,@function
    vm5_cache6_abi_probe:
        push rbx
        push rbp
        push r12
        push r13
        push r14
        push r15
        push rdx
        mov rax, rdi
        mov rdi, rsi
        mov rbx, 0x1122334455667788
        mov rbp, 0x8877665544332211
        mov r12, 0x0123456789abcdef
        mov r13, 0xfedcba9876543210
        mov r14, 0x0f0e0d0c0b0a0908
        mov r15, 0x8070605040302010
        call rax
        mov rdx, [rsp]
        mov [rdx], rbx
        mov [rdx + 8], rbp
        mov [rdx + 16], r12
        mov [rdx + 24], r13
        mov [rdx + 32], r14
        mov [rdx + 40], r15
        add rsp, 8
        pop r15
        pop r14
        pop r13
        pop r12
        pop rbp
        pop rbx
        ret
        .size vm5_cache6_abi_probe, .-vm5_cache6_abi_probe
        "#
    );

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    unsafe extern "C" {
        fn vm5_cache6_abi_probe(
            entry: *const u8,
            context: *mut super::RunContext,
            output: *mut u64,
        );
    }

    fn decoded(machine: &Machine, start: u32, count: usize) -> Vec<BlockInstruction> {
        (0..count)
            .map(|index| machine.fetch_decode(start + index as u32 * 4))
            .collect()
    }

    fn block(machine: &Machine, start: u32, count: usize) -> LinkedBlock {
        LinkedBlock::compile(&decoded(machine, start, count)).unwrap()
    }

    fn relative_target(code: &[u8], displacement_offset: usize, instruction_end: usize) -> usize {
        let displacement = i32::from_le_bytes(
            code[displacement_offset..displacement_offset + 4]
                .try_into()
                .unwrap(),
        );
        usize::try_from(i64::try_from(instruction_end).unwrap() + i64::from(displacement)).unwrap()
    }

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

    fn upper_immediate(opcode: u32, rd: u32, value: u32) -> u32 {
        (value & 0xffff_f000) | (rd << 7) | opcode
    }

    fn immediate(rd: u32, rs1: u32, funct3: u32, immediate: u32) -> u32 {
        ((immediate & 0xfff) << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x13
    }

    fn register(rd: u32, rs1: u32, rs2: u32, funct3: u32, funct7: u32) -> u32 {
        (funct7 << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x33
    }

    fn load(rd: u32, rs1: u32, funct3: u32, immediate: i32) -> u32 {
        ((immediate as u32 & 0xfff) << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x03
    }

    fn store(rs1: u32, rs2: u32, funct3: u32, immediate: i32) -> u32 {
        let immediate = immediate as u32 & 0xfff;
        ((immediate >> 5) << 25)
            | (rs2 << 20)
            | (rs1 << 15)
            | (funct3 << 12)
            | ((immediate & 0x1f) << 7)
            | 0x23
    }

    fn explicit_cache(guests: &[u8]) -> RegisterCache {
        assert!(guests.len() <= super::MAX_CACHED_REGISTERS);
        let mut cache = RegisterCache::empty();
        for (host, &guest) in guests.iter().enumerate() {
            assert!((1..32).contains(&guest));
            assert_eq!(cache.host_by_guest[guest as usize], RegisterCache::NONE);
            cache.guests[host] = guest;
            cache.host_by_guest[guest as usize] = host as u8;
            cache.count += 1;
        }
        cache
    }

    #[test]
    fn direct_operand_encodings_cover_rex_immediates_shifts_and_memory_widths() {
        let mut emitter = Emitter::new(RegisterCache::empty());

        emitter.emit_register_operand(
            BinaryOperation32::Add.opcode(),
            Register32::R12d,
            Operand32::Register(Register32::R13d),
        );
        assert_eq!(emitter.code, [0x45, 0x03, 0xe5]);

        emitter.code.clear();
        emitter.emit_register_operand(
            BinaryOperation32::Add.opcode(),
            Register32::Eax,
            Operand32::GuestMemory(124),
        );
        assert_eq!(emitter.code, [0x03, 0x46, 0x7c]);

        emitter.code.clear();
        emitter.emit_operand_register(&[0x89], Operand32::GuestMemory(20), Register32::R12d);
        assert_eq!(emitter.code, [0x44, 0x89, 0x66, 0x14]);

        let immediate_cases = [
            (
                u32::from_ne_bytes((-129_i32).to_ne_bytes()),
                &[0x81, 0xc5, 0x7f, 0xff, 0xff, 0xff][..],
            ),
            (
                u32::from_ne_bytes((-128_i32).to_ne_bytes()),
                &[0x83, 0xc5, 0x80][..],
            ),
            (127, &[0x83, 0xc5, 0x7f][..]),
            (128, &[0x81, 0xc5, 0x80, 0x00, 0x00, 0x00][..]),
            (255, &[0x81, 0xc5, 0xff, 0x00, 0x00, 0x00][..]),
        ];
        for (value, expected) in immediate_cases {
            emitter.code.clear();
            emitter.emit_group_immediate(0, Operand32::Register(Register32::Ebp), value);
            assert_eq!(emitter.code, expected);
        }

        emitter.code.clear();
        emitter.emit_shift_immediate(5, Operand32::Register(Register32::R15d), 31);
        assert_eq!(emitter.code, [0x41, 0xc1, 0xef, 0x1f]);
        emitter.code.clear();
        emitter.emit_shift_immediate(5, Operand32::Register(Register32::R15d), 1);
        assert_eq!(emitter.code, [0x41, 0xd1, 0xef]);

        let byte_stores = [
            (Register32::Ecx, &[0x41, 0x88, 0x0c, 0x01][..]),
            (Register32::Ebx, &[0x41, 0x88, 0x1c, 0x01][..]),
            (Register32::Ebp, &[0x41, 0x88, 0x2c, 0x01][..]),
            (Register32::R12d, &[0x45, 0x88, 0x24, 0x01][..]),
            (Register32::R13d, &[0x45, 0x88, 0x2c, 0x01][..]),
            (Register32::R14d, &[0x45, 0x88, 0x34, 0x01][..]),
            (Register32::R15d, &[0x45, 0x88, 0x3c, 0x01][..]),
        ];
        for (source, expected) in byte_stores {
            emitter.code.clear();
            emitter.emit_flat_store(source, MemoryWidth::Byte);
            assert_eq!(emitter.code, expected);
        }

        let half_stores = [
            (Register32::Ecx, &[0x66, 0x41, 0x89, 0x0c, 0x01][..]),
            (Register32::Ebx, &[0x66, 0x41, 0x89, 0x1c, 0x01][..]),
            (Register32::Ebp, &[0x66, 0x41, 0x89, 0x2c, 0x01][..]),
            (Register32::R12d, &[0x66, 0x45, 0x89, 0x24, 0x01][..]),
            (Register32::R13d, &[0x66, 0x45, 0x89, 0x2c, 0x01][..]),
            (Register32::R14d, &[0x66, 0x45, 0x89, 0x34, 0x01][..]),
            (Register32::R15d, &[0x66, 0x45, 0x89, 0x3c, 0x01][..]),
        ];
        let word_stores = [
            (Register32::Ecx, &[0x41, 0x89, 0x0c, 0x01][..]),
            (Register32::Ebx, &[0x41, 0x89, 0x1c, 0x01][..]),
            (Register32::Ebp, &[0x41, 0x89, 0x2c, 0x01][..]),
            (Register32::R12d, &[0x45, 0x89, 0x24, 0x01][..]),
            (Register32::R13d, &[0x45, 0x89, 0x2c, 0x01][..]),
            (Register32::R14d, &[0x45, 0x89, 0x34, 0x01][..]),
            (Register32::R15d, &[0x45, 0x89, 0x3c, 0x01][..]),
        ];
        for (width, cases) in [
            (MemoryWidth::Half, &half_stores[..]),
            (MemoryWidth::Word, &word_stores[..]),
        ] {
            for &(source, expected) in cases {
                emitter.code.clear();
                emitter.emit_flat_store(source, width);
                assert_eq!(emitter.code, expected);
            }
        }

        let load_destinations = [
            (Register32::Eax, 0x41, 0x04),
            (Register32::Ebx, 0x41, 0x1c),
            (Register32::Ebp, 0x41, 0x2c),
            (Register32::R12d, 0x45, 0x24),
            (Register32::R13d, 0x45, 0x2c),
            (Register32::R14d, 0x45, 0x34),
            (Register32::R15d, 0x45, 0x3c),
        ];
        let load_operations = [
            (MemoryWidth::Byte, true, &[0x0f, 0xbe][..]),
            (MemoryWidth::Byte, false, &[0x0f, 0xb6][..]),
            (MemoryWidth::Half, true, &[0x0f, 0xbf][..]),
            (MemoryWidth::Half, false, &[0x0f, 0xb7][..]),
            (MemoryWidth::Word, false, &[0x8b][..]),
        ];
        for (width, signed, opcode) in load_operations {
            for (destination, rex, modrm) in load_destinations {
                let mut expected = vec![rex];
                expected.extend_from_slice(opcode);
                expected.extend_from_slice(&[modrm, 0x01]);
                emitter.code.clear();
                emitter.emit_flat_load(destination, width, signed);
                assert_eq!(emitter.code, expected);
            }
        }
    }

    #[test]
    fn flat_memory_validation_uses_full_rv32_permissions_and_exact_alignment() {
        for (width, permission, alignment_mask) in [
            (MemoryWidth::Byte, 1_u8, 0_u32),
            (MemoryWidth::Half, 2_u8, 1_u32),
            (MemoryWidth::Word, 1_u8, 3_u32),
        ] {
            let mut emitter = Emitter::new(RegisterCache::empty());
            let failures = emitter
                .checked_memory_address(0, 0, width, permission)
                .unwrap();

            let mut expected = vec![0x31, 0xc0]; // xor eax, eax
            if alignment_mask != 0 {
                expected.extend_from_slice(&[0xa8, alignment_mask as u8]); // test al, mask
                expected.extend_from_slice(&[0x0f, 0x85, 0, 0, 0, 0]); // jnz slow
            }
            expected.extend_from_slice(&[
                0x89, 0xc2, // mov edx, eax
                0xc1, 0xea, 0x0c, // shr edx, PAGE_SHIFT
                0x41, 0xf6, 0x04, 0x10, permission, // test [r8+rdx], permission
                0x0f, 0x84, 0, 0, 0, 0, // jz precise slow path
            ]);
            assert_eq!(emitter.code, expected);
            if alignment_mask == 0 {
                assert_eq!(failures.len(), 1);
                assert_eq!(failures[0].displacement_offset, 14);
                assert_eq!(failures[0].instruction_end, 18);
            } else {
                assert_eq!(failures.len(), 2);
                assert_eq!(failures[0].displacement_offset, 6);
                assert_eq!(failures[0].instruction_end, 10);
                assert_eq!(failures[1].displacement_offset, 22);
                assert_eq!(failures[1].instruction_end, 26);
            }
        }
    }

    #[test]
    fn private_support_boundary_matches_private_compilation() {
        let code = [
            upper_immediate(0x37, 5, 0x8123_4000),
            upper_immediate(0x17, 5, 0xffff_f000),
            jal(5, 8),
            branch(0, 5, 6, 8),
            branch(1, 5, 6, 8),
            branch(4, 5, 6, 8),
            branch(5, 5, 6, 8),
            branch(6, 5, 6, 8),
            branch(7, 5, 6, 8),
            addi(5, 6, -1),
            immediate(5, 6, 2, 0xfff),
            immediate(5, 6, 3, 0xfff),
            immediate(5, 6, 4, 0x55a),
            immediate(5, 6, 6, 0x055),
            immediate(5, 6, 7, 0x0ff),
            immediate(5, 6, 1, 31),
            immediate(5, 6, 5, 31),
            immediate(5, 6, 5, (0x20 << 5) | 31),
            register(5, 6, 7, 0, 0),
            register(5, 6, 7, 0, 0x20),
            register(5, 6, 7, 1, 0),
            register(5, 6, 7, 2, 0),
            register(5, 6, 7, 3, 0),
            register(5, 6, 7, 4, 0),
            register(5, 6, 7, 5, 0),
            register(5, 6, 7, 5, 0x20),
            register(5, 6, 7, 6, 0),
            register(5, 6, 7, 7, 0),
            register(5, 6, 7, 0, 1),
            register(5, 6, 7, 1, 1),
            register(5, 6, 7, 2, 1),
            register(5, 6, 7, 3, 1),
            register(5, 6, 7, 4, 1),
            register(5, 6, 7, 5, 1),
            register(5, 6, 7, 6, 1),
            register(5, 6, 7, 7, 1),
            0x0000_000f,
            (1 << 25) | (1 << 12) | (5 << 7) | 0x13,
            (2 << 25) | (7 << 20) | (6 << 15) | (5 << 7) | 0x33,
            (1 << 12) | 0x0f,
            (2 << 12) | 0x63,
            jal(5, 2),
            branch(0, 5, 6, 2),
            lw(6, 0, 0),
            0x0000_0073,
        ];
        let image = image_with_code_at(&code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);

        for index in 0..code.len() {
            let instruction = machine
                .fetch_decode(IMAGE_START + index as u32 * 4)
                .unwrap();
            let staged = LinkedBlock::compile(&[Ok(instruction)]).is_some();
            assert_eq!(staged, LinkedBlock::supports(instruction));
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    fn assert_one_matches_interpreter(instruction: u32, registers: &[(usize, u32)]) {
        let image = image_with_code_at(&[instruction], IMAGE_START);
        let mut expected = Machine::new(&image, &[], 0);
        let mut actual = Machine::new(&image, &[], 0);
        for &(register, value) in registers {
            expected.registers[register] = value;
            actual.registers[register] = value;
        }
        let staged = block(&expected, IMAGE_START, 1);
        let program = LinkedProgram::publish(vec![staged], usize::MAX).unwrap();

        let decoded = expected.fetch_decode(IMAGE_START);
        assert!(expected.execute_one(decoded).is_none());
        let native_run = program.entry(0).unwrap().execute(
            &mut actual.registers,
            &mut actual.memory,
            IMAGE_START,
            1,
        );

        assert_eq!(native_run.retired, 1);
        assert_eq!(native_run.pc, expected.pc);
        assert_eq!(actual.registers, expected.registers);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    fn assert_first_with_selected_cache_matches_interpreter(
        instruction: u32,
        registers: &[(usize, u32)],
    ) {
        let code = vec![instruction; MIN_WEIGHTED_CACHE_ACCESSES as usize];
        let image = image_with_code_at(&code, IMAGE_START);
        let mut expected = Machine::new(&image, &[], 0);
        let mut actual = Machine::new(&image, &[], 0);
        for &(register, value) in registers {
            expected.registers[register] = value;
            actual.registers[register] = value;
        }
        let blocks = (0..code.len())
            .map(|index| block(&expected, IMAGE_START + index as u32 * 4, 1))
            .collect();
        let program = LinkedProgram::publish(blocks, usize::MAX).unwrap();
        assert!(program.cached_register_count() > 0);

        let decoded = expected.fetch_decode(IMAGE_START);
        assert!(expected.execute_one(decoded).is_none());
        let native = program.entry(0).unwrap().execute(
            &mut actual.registers,
            &mut actual.memory,
            IMAGE_START,
            1,
        );

        assert_eq!(native.retired, 1);
        assert_eq!(native.pc, expected.pc);
        assert_eq!(actual.registers, expected.registers);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    fn publish_singletons(machine: &Machine, count: usize) -> LinkedProgram {
        LinkedProgram::publish(
            (0..count)
                .map(|index| block(machine, IMAGE_START + index as u32 * 4, 1))
                .collect(),
            usize::MAX,
        )
        .unwrap()
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn direct_cached_alu_immediate_and_branch_aliases_match_the_interpreter() {
        let binary_operations = [(0, 0), (0, 0x20), (4, 0), (6, 0), (7, 0), (0, 1)];
        for (funct3, funct7) in binary_operations {
            for instruction in [
                register(5, 5, 6, funct3, funct7),
                register(6, 5, 6, funct3, funct7),
                register(5, 5, 5, funct3, funct7),
                register(5, 0, 6, funct3, funct7),
                register(5, 6, 0, funct3, funct7),
            ] {
                assert_first_with_selected_cache_matches_interpreter(
                    instruction,
                    &[(5, 0x8000_0001), (6, 0xffff_fffd)],
                );
            }
        }

        let immediate_cases = [
            addi(5, 5, -128),
            addi(5, 6, 127),
            addi(5, 5, 128),
            immediate(5, 5, 4, 0x55a),
            immediate(5, 6, 6, 0x055),
            immediate(5, 5, 7, 0x0ff),
            immediate(5, 5, 1, 0),
            immediate(5, 5, 1, 31),
            immediate(5, 5, 5, 31),
            immediate(5, 5, 5, (0x20 << 5) | 31),
        ];
        for instruction in immediate_cases {
            assert_first_with_selected_cache_matches_interpreter(
                instruction,
                &[(5, 0x8000_0001), (6, 0x7fff_fffe)],
            );
        }

        let branch_operands = [
            (5, 6, 0x8000_0000, 1),
            (0, 5, 0, 1),
            (5, 0, u32::MAX, 0),
            (5, 5, 0x1234_5678, 0x1234_5678),
        ];
        for funct3 in [0, 1, 4, 5, 6, 7] {
            for (left, right, left_value, right_value) in branch_operands {
                let mut initial = Vec::new();
                if left != 0 {
                    initial.push((left as usize, left_value));
                }
                if right != 0 {
                    initial.push((right as usize, right_value));
                }
                assert_first_with_selected_cache_matches_interpreter(
                    branch(funct3, left, right, 8),
                    &initial,
                );
            }
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn direct_cached_load_destination_preserves_aliases_sparse_pages_and_failures() {
        let resident_cases = [
            (0, 1, 0x80, 0xffff_ff80),
            (4, 1, 0x80, 0x80),
            (1, 2, 0x8001, 0xffff_8001),
            (5, 2, 0x8001, 0x8001),
            (2, 4, 0x89ab_cdef, 0x89ab_cdef),
        ];
        for (funct3, width, value, result) in resident_cases {
            let instruction = load(5, 5, funct3, 0);
            let code = vec![instruction; MIN_WEIGHTED_CACHE_ACCESSES as usize];
            let image = image_with_code_at(&code, IMAGE_START);
            let mut expected = Machine::new(&image, &[], 0);
            let mut actual = Machine::new(&image, &[], 0);
            let address = STACK_START + 0x400;
            expected.registers[5] = address;
            actual.registers[5] = address;
            expected
                .memory
                .store(address, width, value, IMAGE_START)
                .unwrap();
            actual
                .memory
                .store(address, width, value, IMAGE_START)
                .unwrap();
            let program = publish_singletons(&expected, code.len());
            assert_eq!(program.cache.host(5), Some(super::CachedHost::Ebx));

            assert!(
                expected
                    .execute_one(expected.fetch_decode(IMAGE_START))
                    .is_none()
            );
            let native = program.entry(0).unwrap().execute(
                &mut actual.registers,
                &mut actual.memory,
                IMAGE_START,
                1,
            );
            assert_eq!(native.retired, 1);
            assert_eq!(native.pc, expected.pc);
            assert_eq!(actual.registers[5], result);
            assert_eq!(actual.registers, expected.registers);
            #[cfg(feature = "profile")]
            {
                assert_eq!(native.profile.direct_memory_load, 1);
                assert_eq!(native.profile.direct_memory_store, 0);
            }
        }

        let instruction = lw(5, 5, 0);
        let code = vec![instruction; MIN_WEIGHTED_CACHE_ACCESSES as usize];
        let sparse_address = 0x0100_0000;
        let mut sparse_image = image_with_code_at(&code, IMAGE_START);
        sparse_image.permissions[(sparse_address >> PAGE_SHIFT) as usize] = PERM_READ;
        let mut sparse = Machine::new(&sparse_image, &[], 0);
        sparse.registers[5] = sparse_address;
        let sparse_program = publish_singletons(&sparse, code.len());
        let sparse_run = sparse_program.entry(0).unwrap().execute(
            &mut sparse.registers,
            &mut sparse.memory,
            IMAGE_START,
            1,
        );
        assert_eq!(sparse_run.retired, 1);
        assert_eq!(sparse.registers[5], 0);

        for address in [0x0200_0000, STACK_START + 0x401] {
            let image = image_with_code_at(&code, IMAGE_START);
            let mut machine = Machine::new(&image, &[], 0);
            machine.registers[5] = address;
            let program = publish_singletons(&machine, code.len());
            let failed = program.entry(0).unwrap().execute(
                &mut machine.registers,
                &mut machine.memory,
                IMAGE_START,
                1,
            );
            assert_eq!(failed.stop, NativeStop::InterpretOne);
            assert_eq!(failed.retired, 0);
            assert_eq!(failed.pc, IMAGE_START);
            assert_eq!(machine.registers[5], address);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn direct_cached_store_source_covers_bpl_widths_aliases_and_failures() {
        for (funct3, width, value) in [(0, 1, 0xa5), (1, 2, 0xbbaa), (2, 4, 0x4433_2211)] {
            let mut code = vec![addi(4, 4, 0); 6];
            let store_index = code.len();
            code.extend(vec![
                store(10, 5, funct3, 0);
                MIN_WEIGHTED_CACHE_ACCESSES as usize
            ]);
            let image = image_with_code_at(&code, IMAGE_START);
            let mut expected = Machine::new(&image, &[], 0);
            let mut actual = Machine::new(&image, &[], 0);
            let pc = IMAGE_START + store_index as u32 * 4;
            let address = STACK_START + 0x500;
            expected.pc = pc;
            expected.registers[5] = value;
            expected.registers[10] = address;
            actual.registers[5] = value;
            actual.registers[10] = address;
            expected.memory.store(address, 4, 0, pc).unwrap();
            actual.memory.store(address, 4, 0, pc).unwrap();
            let program = publish_singletons(&expected, code.len());
            assert_eq!(program.cache.host(5), Some(super::CachedHost::Ebp));

            assert!(expected.execute_one(expected.fetch_decode(pc)).is_none());
            let native = program.entry(store_index).unwrap().execute(
                &mut actual.registers,
                &mut actual.memory,
                pc,
                1,
            );
            assert_eq!(native.retired, 1);
            assert_eq!(native.pc, expected.pc);
            assert_eq!(actual.registers, expected.registers);
            assert_eq!(
                actual.memory.read(address, width),
                expected.memory.read(address, width)
            );
            #[cfg(feature = "profile")]
            {
                assert_eq!(native.profile.direct_memory_load, 0);
                assert_eq!(native.profile.direct_memory_store, 1);
            }
        }

        let alias_instruction = store(5, 5, 2, 0);
        let alias_code = vec![alias_instruction; MIN_WEIGHTED_CACHE_ACCESSES as usize];
        let alias_image = image_with_code_at(&alias_code, IMAGE_START);
        let mut alias = Machine::new(&alias_image, &[], 0);
        let alias_address = STACK_START + 0x600;
        alias.registers[5] = alias_address;
        alias
            .memory
            .store(alias_address, 4, 0, IMAGE_START)
            .unwrap();
        let alias_program = publish_singletons(&alias, alias_code.len());
        let alias_run = alias_program.entry(0).unwrap().execute(
            &mut alias.registers,
            &mut alias.memory,
            IMAGE_START,
            1,
        );
        assert_eq!(alias_run.retired, 1);
        assert_eq!(
            alias.memory.read(alias_address, 4),
            alias_address.to_le_bytes()
        );

        for address in [0x0200_0000, STACK_START + 0x501] {
            let code = vec![store(10, 5, 1, 0); MIN_WEIGHTED_CACHE_ACCESSES as usize];
            let image = image_with_code_at(&code, IMAGE_START);
            let mut machine = Machine::new(&image, &[], 0);
            machine.registers[5] = 0xbbaa;
            machine.registers[10] = address;
            let program = publish_singletons(&machine, code.len());
            let before = machine.registers;
            let failed = program.entry(0).unwrap().execute(
                &mut machine.registers,
                &mut machine.memory,
                IMAGE_START,
                1,
            );
            assert_eq!(failed.stop, NativeStop::InterpretOne);
            assert_eq!(failed.retired, 0);
            assert_eq!(machine.registers, before);
        }

        let sparse_address = 0x0100_0000;
        let sparse_code = vec![store(10, 5, 2, 0); MIN_WEIGHTED_CACHE_ACCESSES as usize];
        let mut sparse_image = image_with_code_at(&sparse_code, IMAGE_START);
        sparse_image.permissions[(sparse_address >> PAGE_SHIFT) as usize] = PERM_WRITE;
        let mut sparse = Machine::new(&sparse_image, &[], 0);
        sparse.registers[5] = 0xaabb_ccdd;
        sparse.registers[10] = sparse_address;
        let sparse_program = publish_singletons(&sparse, sparse_code.len());
        assert!(sparse_program.cache.host(5).is_some());
        let before = sparse.registers;
        let native = sparse_program.entry(0).unwrap().execute(
            &mut sparse.registers,
            &mut sparse.memory,
            IMAGE_START,
            1,
        );
        assert_eq!(native.stop, NativeStop::Budget);
        assert_eq!(native.retired, 1);
        assert_eq!(native.pc, IMAGE_START + 4);
        assert_eq!(sparse.registers, before);
        #[cfg(feature = "profile")]
        assert_eq!(native.profile.direct_memory_store, 1);
        assert_eq!(
            sparse.memory.read(sparse_address, 4),
            0xaabb_ccdd_u32.to_le_bytes()
        );
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn flat_sparse_store_is_visible_to_fallback_and_the_next_native_run() {
        let code = [store(10, 5, 2, 0), load(6, 10, 2, 0)];
        let address = 0x0100_0000;
        let mut image = image_with_code_at(&code, IMAGE_START);
        image.permissions[(address >> PAGE_SHIFT) as usize] = PERM_READ | PERM_WRITE;
        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[5] = 0x4433_2211;
        machine.registers[10] = address;
        let blocks = vec![
            block(&machine, IMAGE_START, 1),
            block(&machine, IMAGE_START + 4, 1),
        ];
        let program = LinkedProgram::publish(blocks, usize::MAX).unwrap();

        let stored = program.entry(0).unwrap().execute(
            &mut machine.registers,
            &mut machine.memory,
            IMAGE_START,
            1,
        );
        assert_eq!(stored.stop, NativeStop::Budget);
        assert_eq!(stored.retired, 1);
        assert_eq!(stored.pc, IMAGE_START + 4);
        assert_eq!(
            machine.memory.read(address, 4),
            0x4433_2211_u32.to_le_bytes()
        );

        machine.pc = IMAGE_START + 4;
        assert!(
            machine
                .execute_one(machine.fetch_decode(IMAGE_START + 4))
                .is_none()
        );
        assert_eq!(machine.registers[6], 0x4433_2211);

        machine.registers[6] = 0;
        let loaded = program.entry(1).unwrap().execute(
            &mut machine.registers,
            &mut machine.memory,
            IMAGE_START + 4,
            1,
        );
        assert_eq!(loaded.retired, 1);
        assert_eq!(loaded.pc, IMAGE_START + 8);
        assert_eq!(machine.registers[6], 0x4433_2211);
    }

    #[cfg(all(
        feature = "profile",
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn direct_operand_profile_counts_each_executed_lowering_family() {
        let code = [addi(5, 5, 1), register(6, 5, 6, 0, 0), branch(0, 5, 6, 4)];
        let image = image_with_code_at(&code, IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[5] = 1;
        machine.registers[6] = 2;
        let program =
            LinkedProgram::publish(vec![block(&machine, IMAGE_START, 3)], usize::MAX).unwrap();

        let run = program.entry(0).unwrap().execute(
            &mut machine.registers,
            &mut machine.memory,
            IMAGE_START,
            3,
        );

        assert_eq!(run.profile.direct_immediate, 1);
        assert_eq!(run.profile.direct_register, 1);
        assert_eq!(run.profile.direct_branch, 1);
        assert_eq!(run.profile.direct_memory_load, 0);
        assert_eq!(run.profile.direct_memory_store, 0);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    struct Rv32mHarness {
        machine: Machine,
        initial_registers: [u32; 32],
        program: LinkedProgram,
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    impl Rv32mHarness {
        fn new() -> Self {
            let code = (0..8)
                .map(|funct3| register(5, 6, 7, funct3, 1))
                .collect::<Vec<_>>();
            let image = image_with_code_at(&code, IMAGE_START);
            let machine = Machine::new(&image, &[], 0);
            let blocks = (0..code.len())
                .map(|index| block(&machine, IMAGE_START + index as u32 * 4, 1))
                .collect();
            let program = LinkedProgram::publish(blocks, usize::MAX).unwrap();
            let initial_registers = machine.registers;
            Self {
                machine,
                initial_registers,
                program,
            }
        }

        fn assert_case(&mut self, funct3: usize, left: u32, right: u32) {
            let pc = IMAGE_START + funct3 as u32 * 4;
            self.machine.registers = self.initial_registers;
            self.machine.registers[6] = left;
            self.machine.registers[7] = right;
            self.machine.pc = pc;
            self.machine.retired = 0;
            let mut actual_registers = self.machine.registers;

            let decoded = self.machine.fetch_decode(pc);
            assert!(self.machine.execute_one(decoded).is_none());
            let actual = self.program.entry(funct3).unwrap().execute(
                &mut actual_registers,
                &mut self.machine.memory,
                pc,
                1,
            );

            assert_eq!(actual.retired, 1);
            assert_eq!(actual.pc, self.machine.pc);
            assert_eq!(
                actual_registers, self.machine.registers,
                "RV32M funct3={funct3}, left={left:#010x}, right={right:#010x}"
            );
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn rv32m_exhaustive_edge_value_table_matches_the_interpreter() {
        const VALUES: [u32; 17] = [
            0,
            1,
            2,
            3,
            0x0000_ffff,
            0x0001_0000,
            0x3fff_ffff,
            0x4000_0000,
            0x7fff_fffe,
            0x7fff_ffff,
            0x8000_0000,
            0x8000_0001,
            0xbfff_ffff,
            0xffff_0000,
            0xffff_fffd,
            0xffff_fffe,
            0xffff_ffff,
        ];
        let mut harness = Rv32mHarness::new();

        for funct3 in 0..8 {
            for left in VALUES {
                for right in VALUES {
                    harness.assert_case(funct3, left, right);
                }
            }
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn rv32m_deterministic_randomized_differential() {
        fn next(state: &mut u64) -> u32 {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            (*state >> 16) as u32
        }

        let mut harness = Rv32mHarness::new();
        let mut state = 0xd1b5_4a32_d192_ed03;
        for _ in 0..4_096 {
            let left = next(&mut state);
            let right = next(&mut state);
            for funct3 in 0..8 {
                harness.assert_case(funct3, left, right);
            }
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn rv32m_division_corner_cases_never_raise_host_divide_error() {
        let cases = [
            (4, 0x8000_0000, u32::MAX),
            (4, 0x8000_0000, 0),
            (4, 0x7fff_ffff, 0),
            (5, u32::MAX, 0),
            (6, 0x8000_0000, u32::MAX),
            (6, 0x8000_0000, 0),
            (7, u32::MAX, 0),
        ];
        for (funct3, left, right) in cases {
            assert_one_matches_interpreter(register(5, 6, 7, funct3, 1), &[(6, left), (7, right)]);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn rv32m_high_products_cover_signed_and_unsigned_extremes() {
        let cases = [
            (1, 0x8000_0000, 0x8000_0000),
            (1, u32::MAX, u32::MAX),
            (2, 0x8000_0000, u32::MAX),
            (2, u32::MAX, 0x8000_0000),
            (3, 0x8000_0000, 0x8000_0000),
            (3, u32::MAX, u32::MAX),
        ];
        for (funct3, left, right) in cases {
            assert_one_matches_interpreter(register(5, 6, 7, funct3, 1), &[(6, left), (7, right)]);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn rv32m_preserves_operand_aliases_and_x0() {
        for funct3 in 0..8 {
            assert_one_matches_interpreter(
                register(6, 6, 7, funct3, 1),
                &[(6, 0x8000_0001), (7, 0xffff_fffd)],
            );
            assert_one_matches_interpreter(
                register(7, 6, 7, funct3, 1),
                &[(6, 0x8000_0001), (7, 3)],
            );
            assert_one_matches_interpreter(register(6, 6, 6, funct3, 1), &[(6, 0x8000_0001)]);
            assert_one_matches_interpreter(
                register(0, 6, 7, funct3, 1),
                &[(6, 0x8000_0001), (7, 0)],
            );
            assert_one_matches_interpreter(register(5, 0, 7, funct3, 1), &[(7, 3)]);
            assert_one_matches_interpreter(register(5, 6, 0, funct3, 1), &[(6, 0x8000_0001)]);
        }
    }

    #[cfg(all(
        feature = "profile",
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn rv32m_generated_register_traffic_is_exact() {
        let code = (0..8)
            .map(|funct3| register(5, 6, 7, funct3, 1))
            .collect::<Vec<_>>();
        let image = image_with_code_at(&code, IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        let program =
            LinkedProgram::publish(vec![block(&machine, IMAGE_START, code.len())], usize::MAX)
                .unwrap();
        let mut registers = machine.registers;
        registers[6] = 0x8000_0001;
        registers[7] = 3;

        let result = program.entry(0).unwrap().execute(
            &mut registers,
            &mut machine.memory,
            IMAGE_START,
            code.len() as u64,
        );

        assert_eq!(result.retired, code.len() as u64);
        assert_eq!(result.profile.blocks, 1);
        assert_eq!(result.profile.register_loads, 3);
        assert_eq!(result.profile.register_stores, 3);
        assert_eq!(result.profile.cache_fills, 3);
        assert_eq!(result.profile.cache_spills, 3);
        assert_eq!(result.profile.cache_read_hits, 16);
        assert_eq!(result.profile.cache_write_hits, 8);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn every_lowering_matches_the_interpreter() {
        assert_one_matches_interpreter(upper_immediate(0x37, 5, 0x8123_4000), &[]);
        assert_one_matches_interpreter(upper_immediate(0x17, 5, 0xffff_f000), &[]);
        assert_one_matches_interpreter(0x0000_000f, &[]);
        assert_one_matches_interpreter(jal(5, 8), &[]);
        assert_one_matches_interpreter(jal(0, 8), &[]);

        let immediate_cases = [
            (addi(5, 6, -1), 0),
            (immediate(5, 6, 2, 0xfff), 0x8000_0000),
            (immediate(5, 6, 3, 0xfff), 0xffff_fffe),
            (immediate(5, 6, 4, 0x55a), 0xaa55_aa55),
            (immediate(5, 6, 6, 0x055), 0xaa00_aa00),
            (immediate(5, 6, 7, 0x0ff), 0xaa55_aa55),
            (immediate(5, 6, 1, 31), 1),
            (immediate(5, 6, 5, 31), 0x8000_0000),
            (immediate(5, 6, 5, (0x20 << 5) | 31), 0x8000_0000),
            (addi(0, 6, 1), 9),
        ];
        for (instruction, source) in immediate_cases {
            assert_one_matches_interpreter(instruction, &[(6, source)]);
        }

        let register_cases = [
            (register(5, 6, 7, 0, 0), u32::MAX, 1),
            (register(5, 6, 7, 0, 0x20), 0, 1),
            (register(5, 6, 7, 1, 0), 1, 33),
            (register(5, 6, 7, 2, 0), u32::MAX, 0),
            (register(5, 6, 7, 3, 0), 0, 1),
            (register(5, 6, 7, 4, 0), 0xaa55_aa55, 0x0f0f_0f0f),
            (register(5, 6, 7, 5, 0), 0x8000_0000, 31),
            (register(5, 6, 7, 5, 0x20), 0x8000_0000, 31),
            (register(5, 6, 7, 6, 0), 0xaa00_aa00, 0x0055_0055),
            (register(5, 6, 7, 7, 0), 0xaa55_aa55, 0x0f0f_0f0f),
            (register(5, 6, 7, 0, 1), u32::MAX, 2),
        ];
        for (instruction, left, right) in register_cases {
            assert_one_matches_interpreter(instruction, &[(6, left), (7, right)]);
        }
        assert_one_matches_interpreter(register(5, 5, 5, 0, 0), &[(5, 0x8000_0001)]);
        assert_one_matches_interpreter(register(0, 6, 7, 0, 0), &[(6, 1), (7, 2)]);

        let branch_cases = [
            (0, (5, 5), (5, 6)),
            (1, (5, 6), (5, 5)),
            (4, (u32::MAX, 0), (0, u32::MAX)),
            (5, (0, u32::MAX), (u32::MAX, 0)),
            (6, (0, 1), (1, 0)),
            (7, (1, 0), (0, 1)),
        ];
        for (funct3, taken, not_taken) in branch_cases {
            let instruction = branch(funct3, 6, 7, 8);
            assert_one_matches_interpreter(instruction, &[(6, taken.0), (7, taken.1)]);
            assert_one_matches_interpreter(instruction, &[(6, not_taken.0), (7, not_taken.1)]);
        }
    }

    #[test]
    fn mapping_budget_is_page_rounded() {
        assert_eq!(mapping_length(0, 4_096, usize::MAX), None);
        assert_eq!(mapping_length(1, 4_096, 4_095), None);
        assert_eq!(mapping_length(1, 4_096, 4_096), Some(4_096));
        assert_eq!(mapping_length(4_097, 4_096, 8_192), Some(8_192));
        assert_eq!(mapping_length(usize::MAX, 4_096, usize::MAX), None);
    }

    fn scoring_block(pc: u32, instructions: Vec<super::Lowering>) -> LinkedBlock {
        LinkedBlock {
            pc,
            instructions,
            flow: super::BlockFlow::Fallthrough {
                pc: pc.wrapping_add(4),
            },
            reserved_code_len: 0,
        }
    }

    fn scored_add(destination: usize, source: usize) -> super::Lowering {
        super::Lowering::Immediate {
            destination,
            source,
            operation: super::ImmediateOperation::Add(1),
        }
    }

    #[test]
    fn register_cache_scoring_deduplicates_overlaps_and_weights_nested_loops() {
        let outside = scoring_block(96, vec![scored_add(1, 2)]);
        let inner = scoring_block(
            100,
            vec![
                scored_add(3, 4),
                scored_add(5, 6),
                super::Lowering::Branch {
                    left: 0,
                    right: 0,
                    condition: super::Condition::Equal,
                    fallthrough: 112,
                    target: 104,
                },
            ],
        );
        let duplicate_inner = scoring_block(100, inner.instructions.clone());
        let outer = scoring_block(
            112,
            vec![super::Lowering::Branch {
                left: 0,
                right: 0,
                condition: super::Condition::Equal,
                fallthrough: 116,
                target: 100,
            }],
        );

        let scores = RegisterCache::scores(&[outside, inner, duplicate_inner, outer]);

        assert_eq!(scores[2], 2); // outside every loop: W=1
        assert_eq!(scores[4], 16); // outer loop only: W=8
        assert_eq!(scores[6], 30); // nested inner+outer loops: W=15
    }

    #[test]
    fn register_cache_scoring_excludes_calls_forward_edges_and_x0() {
        let before = scoring_block(100, vec![scored_add(1, 2)]);
        let backward_call = scoring_block(
            104,
            vec![super::Lowering::Jump {
                destination: 1,
                link: 108,
                target: 100,
            }],
        );
        let forward_branch = scoring_block(
            108,
            vec![super::Lowering::Branch {
                left: 0,
                right: 0,
                condition: super::Condition::Equal,
                fallthrough: 112,
                target: 120,
            }],
        );

        let scores = RegisterCache::scores(&[before, backward_call, forward_branch]);

        assert_eq!(scores[0], 0);
        assert_eq!(scores[2], 2);
        assert_eq!(scores[1], 2); // one add write plus one call-link write
    }

    #[test]
    fn register_cache_rejects_a_lone_write_and_a_lone_read() {
        let lone_write = scoring_block(
            0,
            vec![super::Lowering::WriteImmediate {
                destination: 5,
                value: 1,
            }],
        );
        let lone_read = scoring_block(
            4,
            vec![super::Lowering::IndirectJump {
                pc: 4,
                destination: 0,
                source: 6,
                immediate: 0,
                link: 8,
            }],
        );

        assert_eq!(RegisterCache::select(&[lone_write]).count(), 0);
        assert_eq!(RegisterCache::select(&[lone_read]).count(), 0);
    }

    #[test]
    fn register_cache_requires_five_weighted_body_accesses() {
        let four = scoring_block(0, vec![scored_add(6, 5); 4]);
        let five = scoring_block(0, vec![scored_add(6, 5); 5]);

        assert_eq!(RegisterCache::select(&[four]).count(), 0);
        let cache = RegisterCache::select(&[five]);
        assert_eq!(cache.count(), 2);
        assert_eq!(cache.guests()[..2], [5, 6]);
    }

    #[test]
    fn one_read_and_write_in_a_backward_loop_clear_the_cache_gate() {
        let loop_block = scoring_block(
            100,
            vec![
                scored_add(1, 2),
                super::Lowering::Branch {
                    left: 0,
                    right: 0,
                    condition: super::Condition::Equal,
                    fallthrough: 108,
                    target: 100,
                },
            ],
        );

        let cache = RegisterCache::select(&[loop_block]);

        assert_eq!(cache.count(), 2);
        assert_eq!(cache.guests()[..2], [2, 1]);
    }

    #[test]
    fn register_cache_selection_is_bounded_and_breaks_ties_by_guest_index() {
        let block = scoring_block(
            0,
            (1..=7)
                .flat_map(|destination| {
                    (0..MIN_WEIGHTED_CACHE_ACCESSES).map(move |_| super::Lowering::WriteImmediate {
                        destination,
                        value: destination as u32,
                    })
                })
                .collect(),
        );

        let cache = RegisterCache::select(&[block]);

        assert_eq!(cache.count(), super::MAX_CACHED_REGISTERS);
        assert_eq!(
            &cache.guests()[..super::MAX_CACHED_REGISTERS],
            &(1..=super::MAX_CACHED_REGISTERS as u8).collect::<Vec<_>>()
        );
        assert_eq!(RegisterCache::select(&[]).count(), 0);
    }

    #[cfg(not(feature = "profile"))]
    #[test]
    fn region_local_cache_selects_profitable_uncached_reuse_and_skips_jalr() {
        let instructions = vec![scored_add(7, 7); 3];
        let cache = explicit_cache(&[1, 2, 3, 4, 5, 6]);
        let flow = super::BlockFlow::Fallthrough { pc: 12 };
        let mut emitter = Emitter::new(cache);

        emitter.emit_block(&instructions, flow, 0).unwrap();

        assert_eq!(emitter.local_guest, Some(7));
        let fill = [0x44, 0x8b, 0x5e, super::register_offset(7)];
        let spill = [0x44, 0x89, 0x5e, super::register_offset(7)];
        assert_eq!(
            emitter
                .code
                .windows(fill.len())
                .filter(|bytes| *bytes == fill)
                .count(),
            1
        );
        assert_eq!(
            emitter
                .code
                .windows(spill.len())
                .filter(|bytes| *bytes == spill)
                .count(),
            1
        );

        emitter.select_local_cache(
            &instructions,
            super::BlockFlow::IndirectJump { target_hint: None },
        );
        assert_eq!(emitter.local_guest, None);
    }

    #[test]
    fn register_cache_scoring_stays_bounded_at_the_full_block_limit() {
        let blocks = (0..super::MAX_LINKED_BLOCKS)
            .map(|index| {
                scoring_block(
                    u32::try_from(index * 64 * 4).unwrap(),
                    vec![scored_add(1, 2); 64],
                )
            })
            .collect::<Vec<_>>();

        let scores = RegisterCache::scores(&blocks);

        assert_eq!(scores[1], (super::MAX_LINKED_BLOCKS * 64) as u64);
        assert_eq!(scores[2], (super::MAX_LINKED_BLOCKS * 64 * 2) as u64);
    }

    #[test]
    fn full_register_cache_entry_and_exit_match_the_fixed_maxima() {
        let block = scoring_block(
            IMAGE_START,
            (1..=super::MAX_CACHED_REGISTERS)
                .flat_map(|destination| {
                    (0..MIN_WEIGHTED_CACHE_ACCESSES).map(move |_| super::Lowering::WriteImmediate {
                        destination,
                        value: destination as u32,
                    })
                })
                .collect(),
        );
        let cache = RegisterCache::select(std::slice::from_ref(&block));
        let mut emitter = Emitter::new(cache);
        emitter
            .emit_block(&block.instructions, block.flow, block.pc)
            .unwrap();

        let resolved = emitter.resolve().unwrap();

        assert_eq!(resolved.shared_prologue_bytes, MAX_SHARED_PROLOGUE_BYTES);
        assert_eq!(resolved.exit_trampoline_bytes, MAX_EXIT_TRAMPOLINE_BYTES);
        assert_eq!(
            resolved.shared_prologue_bytes + resolved.exit_trampoline_bytes,
            MAX_FIXED_CODE_BYTES
        );
    }

    #[test]
    fn zero_and_partial_cache_entry_exit_sizes_and_layout_are_exact() {
        let no_cache_block = scoring_block(
            IMAGE_START,
            vec![super::Lowering::WriteImmediate {
                destination: 1,
                value: 1,
            }],
        );
        let mut no_cache_emitter = Emitter::new(RegisterCache::empty());
        no_cache_emitter
            .emit_block(
                &no_cache_block.instructions,
                no_cache_block.flow,
                no_cache_block.pc,
            )
            .unwrap();
        let no_cache = no_cache_emitter.resolve().unwrap();

        let no_cache_entry = no_cache.entries[0].1;
        assert_eq!(no_cache_entry.external_offset, 0);
        assert_eq!(no_cache_entry.indirect_offset, 19);
        assert_eq!(no_cache_entry.hot_offset, 23);
        assert_eq!(&no_cache.code[..4], &ENTRY_BYTES);
        assert_eq!(
            &no_cache.code[4..19],
            &[
                0x48, 0x8b, 0x37, // mov rsi, [rdi]
                0x4c, 0x8b, 0x47, 0x18, // mov r8, [rdi+24]
                0x4c, 0x8b, 0x4f, 0x20, // mov r9, [rdi+32]
                0x4c, 0x8b, 0x57, 0x08, // mov r10, [rdi+8]
            ]
        );
        assert_eq!(&no_cache.code[19..23], &ENTRY_BYTES);

        let one_cache_block = scoring_block(
            IMAGE_START,
            vec![
                super::Lowering::WriteImmediate {
                    destination: 1,
                    value: 1,
                };
                MIN_WEIGHTED_CACHE_ACCESSES as usize
            ],
        );
        let one_cache = RegisterCache::select(std::slice::from_ref(&one_cache_block));
        assert_eq!(one_cache.count(), 1);
        let mut one_cache_emitter = Emitter::new(one_cache);
        one_cache_emitter
            .emit_block(
                &one_cache_block.instructions,
                one_cache_block.flow,
                one_cache_block.pc,
            )
            .unwrap();
        let one_cache = one_cache_emitter.resolve().unwrap();

        let profile_counter_bytes = if cfg!(feature = "profile") { 8 } else { 0 };
        assert_eq!(no_cache.external_thunk_bytes, 0);
        assert_eq!(no_cache.shared_prologue_bytes, 0);
        assert_eq!(no_cache.exit_trampoline_bytes, 33);
        assert_eq!(
            no_cache.hot_code_bytes + no_cache.cold_code_bytes,
            no_cache.code.len()
        );
        assert_eq!(one_cache.external_thunk_bytes, EXTERNAL_THUNK_BYTES);
        assert_eq!(one_cache.shared_prologue_bytes, 22 + profile_counter_bytes);
        assert_eq!(one_cache.exit_trampoline_bytes, 37 + profile_counter_bytes);
        assert!(one_cache.shared_prologue_bytes <= MAX_SHARED_PROLOGUE_BYTES);
        assert!(one_cache.exit_trampoline_bytes <= MAX_EXIT_TRAMPOLINE_BYTES);
    }

    #[test]
    fn uncached_inline_entry_reservation_bounds_cached_and_uncached_images() {
        for words in [vec![addi(5, 5, 1)], vec![addi(5, 5, 1); 3]] {
            let image = image_with_code_at(&words, IMAGE_START);
            let machine = Machine::new(&image, &[], 0);
            let block = block(&machine, IMAGE_START, words.len());
            let reserved = MAX_FIXED_CODE_BYTES + block.reserved_code_len();
            let cache = RegisterCache::select(std::slice::from_ref(&block));
            assert_eq!(cache.is_empty(), words.len() == 1);

            let mut emitter = Emitter::new(cache);
            emitter
                .emit_block(&block.instructions, block.flow, block.pc)
                .unwrap();
            let resolved = emitter.resolve().unwrap();

            assert!(resolved.code.len() <= reserved);
            if cache.is_empty() {
                assert_eq!(resolved.external_thunk_bytes, 0);
                assert_eq!(resolved.shared_prologue_bytes, 0);
                assert_eq!(
                    resolved.code.len() + MAX_FIXED_CODE_BYTES - resolved.exit_trampoline_bytes,
                    reserved
                );
            } else {
                assert_eq!(resolved.external_thunk_bytes, EXTERNAL_THUNK_BYTES);
                assert!(resolved.shared_prologue_bytes > 0);
            }
        }
    }

    #[test]
    fn uncached_reservation_bounds_every_direct_family_for_all_cache_sizes() {
        let words = [
            upper_immediate(0x37, 5, 0x8123_4000),
            upper_immediate(0x17, 10, 0xffff_f000),
            addi(5, 5, -128),
            addi(6, 5, 128),
            immediate(7, 7, 4, 0x55a),
            immediate(8, 7, 6, 0x055),
            immediate(9, 9, 7, 0x0ff),
            immediate(10, 10, 1, 31),
            immediate(5, 5, 5, (0x20 << 5) | 31),
            register(5, 5, 6, 0, 0),
            register(6, 5, 6, 0, 0x20),
            register(7, 7, 8, 4, 0),
            register(8, 9, 8, 6, 0),
            register(9, 9, 10, 7, 0),
            register(10, 10, 5, 0, 1),
            load(5, 5, 0, 127),
            load(6, 6, 5, -128),
            load(10, 10, 2, 2_047),
            load(0, 5, 2, 0),
            load(5, 0, 0, 0),
            store(5, 6, 0, -128),
            store(7, 8, 1, 127),
            store(9, 10, 2, -2_048),
            store(5, 0, 2, 0),
            branch(0, 5, 6, 4),
            branch(1, 7, 0, 4),
            branch(4, 0, 8, 4),
            branch(5, 9, 10, 4),
            branch(6, 5, 0, 4),
            branch(7, 0, 6, 4),
        ];
        let image = image_with_code_at(&words, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let blocks = (0..words.len())
            .map(|index| block(&machine, IMAGE_START + index as u32 * 4, 1))
            .collect::<Vec<_>>();
        let reserved = blocks.iter().fold(MAX_FIXED_CODE_BYTES, |total, block| {
            total + block.reserved_code_len()
        });
        let guests = [5, 6, 7, 8, 9, 10];

        for count in 0..=guests.len() {
            let cache = explicit_cache(&guests[..count]);
            let mut emitter = Emitter::new(cache);
            for block in &blocks {
                emitter
                    .emit_block(&block.instructions, block.flow, block.pc)
                    .unwrap();
            }
            let resolved = emitter.resolve().unwrap();
            assert!(
                resolved.code.len() <= reserved,
                "cache size {count} emitted {} bytes beyond reservation {reserved}",
                resolved.code.len()
            );
        }

        fn visit_ordered_partial_mappings<F>(values: &mut [u8], index: usize, visit: &mut F)
        where
            F: FnMut(&[u8]),
        {
            visit(&values[..index]);
            if index == values.len() {
                return;
            }
            for candidate in index..values.len() {
                values.swap(index, candidate);
                visit_ordered_partial_mappings(values, index + 1, visit);
                values.swap(index, candidate);
            }
        }

        let mut permutation = guests;
        visit_ordered_partial_mappings(&mut permutation, 0, &mut |mapping| {
            let cache = explicit_cache(mapping);
            for block in &blocks {
                let mut emitter = Emitter::new(cache);
                emitter
                    .emit_block(&block.instructions, block.flow, block.pc)
                    .unwrap();
                let cached_reserved = emitter.reserved_code_len().unwrap();
                assert!(
                    cached_reserved <= block.reserved_code_len(),
                    "mapping {mapping:?} emitted {cached_reserved} reserved bytes for {:#x}, empty-cache reservation {}",
                    block.pc,
                    block.reserved_code_len()
                );
            }
        });
    }

    #[test]
    fn every_external_and_indirect_entry_is_a_cet_landing_pad() {
        let code = [
            addi(5, 5, 1),
            addi(5, 5, 1),
            addi(5, 5, 1),
            addi(6, 6, 1),
            addi(6, 6, 1),
            addi(6, 6, 1),
        ];
        let image = image_with_code_at(&code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let blocks = [
            block(&machine, IMAGE_START, 3),
            block(&machine, IMAGE_START + 12, 3),
        ];
        let cache = RegisterCache::select(&blocks);
        assert_eq!(cache.count(), 2);
        let mut emitter = super::Emitter::new(cache);
        for block in &blocks {
            emitter
                .emit_block(&block.instructions, block.flow, block.pc)
                .unwrap();
        }
        let hot_len = emitter.code.len();
        let resolved = emitter.resolve().unwrap();
        assert_eq!(resolved.hot_code_bytes, hot_len);
        assert_eq!(
            resolved.hot_code_bytes + resolved.cold_code_bytes,
            resolved.code.len()
        );
        let prologue = hot_len + resolved.external_thunk_bytes;
        for (_, entry) in &resolved.entries {
            assert!(entry.indirect_offset < hot_len);
            assert!(entry.external_offset >= hot_len);
            assert_eq!(
                &resolved.code[entry.external_offset..entry.external_offset + ENTRY_BYTES.len()],
                &ENTRY_BYTES
            );
            assert_eq!(
                &resolved.code[entry.indirect_offset..entry.hot_offset],
                &ENTRY_BYTES
            );
            assert_eq!(
                resolved.code[entry.external_offset + 4..entry.external_offset + 7],
                [0x4c, 0x8d, 0x1d]
            );
            assert_eq!(
                relative_target(
                    &resolved.code,
                    entry.external_offset + 7,
                    entry.external_offset + 11,
                ),
                entry.indirect_offset
            );
            assert_eq!(resolved.code[entry.external_offset + 11], 0xe9);
            assert_eq!(
                relative_target(
                    &resolved.code,
                    entry.external_offset + 12,
                    entry.external_offset + EXTERNAL_THUNK_BYTES,
                ),
                prologue
            );
        }
        assert_eq!(
            resolved.external_thunk_bytes,
            blocks.len() * EXTERNAL_THUNK_BYTES
        );
    }

    #[test]
    fn adjacent_direct_edges_share_their_slots_with_cet_pads() {
        let code = [addi(5, 5, 1), addi(5, 5, 1), addi(5, 5, 1)];
        let image = image_with_code_at(&code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let blocks = [
            block(&machine, IMAGE_START, 1),
            block(&machine, IMAGE_START + 4, 2),
        ];
        let cache = RegisterCache::select(&blocks);
        assert_eq!(cache.count(), 1);
        let mut emitter = Emitter::new(cache);
        for block in &blocks {
            emitter
                .emit_block(&block.instructions, block.flow, block.pc)
                .unwrap();
        }
        let first_edge = emitter.edges[0];
        let resolved = emitter.resolve().unwrap();
        let code = resolved.code;
        let entries = resolved.entries;
        let table = DispatchTable::build(&code, &entries).unwrap();

        assert_eq!(table.page_count(), 1);
        assert_eq!(table.entry_count(), 2);
        assert_eq!(
            table.bytes(),
            super::PAGE_COUNT * size_of::<usize>()
                + super::PAGE_SIZE
                + size_of::<Box<[u32; super::INSTRUCTIONS_PER_PAGE]>>()
        );
        for &(pc, entry) in &entries {
            assert_eq!(
                table.encoded_entry(pc),
                Some(u32::try_from(entry.indirect_offset).unwrap() + 1)
            );
            assert_eq!(
                &code[entry.indirect_offset..entry.indirect_offset + ENTRY_BYTES.len()],
                &ENTRY_BYTES
            );
        }
        assert_eq!(table.encoded_entry(IMAGE_START + 8), Some(0));

        let edge_end = first_edge.slot_offset + EDGE_SLOT_BYTES;
        assert_eq!(edge_end, entries[1].1.hot_offset);
        assert_eq!(entries[1].1.indirect_offset + ENTRY_BYTES.len(), edge_end);
        #[cfg(feature = "profile")]
        assert_eq!(
            &code[first_edge.slot_offset..edge_end],
            &[
                0x48,
                0xff,
                0x47,
                u8::try_from(super::PROFILE_DIRECT_LINKS_OFFSET).unwrap(),
                0x90,
                0xf3,
                0x0f,
                0x1e,
                0xfa,
            ]
        );
        #[cfg(not(feature = "profile"))]
        assert_eq!(
            &code[first_edge.slot_offset..edge_end],
            &[0x90, 0xf3, 0x0f, 0x1e, 0xfa]
        );
        assert_ne!(entries[1].1.hot_offset, entries[1].1.indirect_offset);
    }

    #[test]
    fn dispatch_table_validates_keys_landings_and_sparse_page_bounds() {
        let mut code = vec![0; ENTRY_BYTES.len() * 2];
        code[..ENTRY_BYTES.len()].copy_from_slice(&ENTRY_BYTES);
        code[ENTRY_BYTES.len()..].copy_from_slice(&ENTRY_BYTES);
        let first = super::EntryMetadata {
            external_offset: 0,
            indirect_offset: 0,
            hot_offset: 0,
        };
        let second = super::EntryMetadata {
            external_offset: ENTRY_BYTES.len(),
            indirect_offset: ENTRY_BYTES.len(),
            hot_offset: ENTRY_BYTES.len(),
        };
        let next_page = IMAGE_START + super::PAGE_SIZE as u32;
        let entries = [(IMAGE_START, first), (next_page, second)];

        let table = DispatchTable::build(&code, &entries).unwrap();

        assert_eq!(table.page_count(), 2);
        assert_eq!(table.entry_count(), 2);
        assert_eq!(
            table.bytes(),
            super::PAGE_COUNT * size_of::<usize>()
                + 2 * super::PAGE_SIZE
                + 2 * size_of::<Box<[u32; super::INSTRUCTIONS_PER_PAGE]>>()
        );
        assert!(table.bytes() <= super::MAX_DISPATCH_BYTES);
        assert_eq!(
            super::MAX_DISPATCH_BYTES,
            super::PAGE_COUNT * size_of::<usize>()
                + super::MAX_LINKED_BLOCKS
                    * (super::PAGE_SIZE + size_of::<Box<[u32; super::INSTRUCTIONS_PER_PAGE]>>())
        );
        assert_eq!(table.encoded_entry(IMAGE_START), Some(1));
        assert_eq!(
            table.encoded_entry(next_page),
            Some(u32::try_from(ENTRY_BYTES.len()).unwrap() + 1)
        );
        assert_eq!(table.encoded_entry(IMAGE_START + 4), Some(0));
        assert_eq!(
            table.encoded_entry(IMAGE_START + 2 * super::PAGE_SIZE as u32),
            Some(0)
        );

        assert!(
            DispatchTable::build(&code, &[(IMAGE_START, first), (IMAGE_START, second)]).is_none()
        );
        assert!(DispatchTable::build(&code, &[(IMAGE_START + 2, first)]).is_none());
        assert!(DispatchTable::build(&code, &[(super::ADDRESS_SPACE_SIZE, first)]).is_none());

        let mut invalid_code = code.clone();
        invalid_code[first.indirect_offset] ^= 1;
        assert!(DispatchTable::build(&invalid_code, &[(IMAGE_START, first)]).is_none());
        let beyond_code = super::EntryMetadata {
            indirect_offset: code.len(),
            ..first
        };
        assert!(DispatchTable::build(&code, &[(IMAGE_START, beyond_code)]).is_none());
    }

    #[cfg(feature = "profile")]
    #[test]
    fn profile_context_offsets_use_disp32_beyond_the_signed_byte_range() {
        let mut emitter = Emitter::new(RegisterCache::empty());

        emitter.increment_context(127);
        emitter.increment_context(128);
        emitter.add_context(136, 1).unwrap();

        assert_eq!(
            emitter.code,
            [
                0x48, 0xff, 0x47, 0x7f, // inc qword ptr [rdi+127]
                0x48, 0xff, 0x87, 0x80, 0x00, 0x00, 0x00, // [rdi+128]
                0x48, 0x81, 0x87, 0x88, 0x00, 0x00, 0x00, // add [rdi+136]
                0x01, 0x00, 0x00, 0x00,
            ]
        );
    }

    #[test]
    fn jalr_slow_paths_relocate_to_precise_and_shared_committed_veneers() {
        let image = image_with_code_at(&[jalr(5, 6, -8)], IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let block = block(&machine, IMAGE_START, 1);
        let mut emitter = Emitter::new(RegisterCache::empty());
        emitter
            .emit_block(&block.instructions, block.flow, block.pc)
            .unwrap();
        let hot_len = emitter.code.len();
        let misaligned = emitter.interpret_one_exits[0].branches[0];
        let misses = emitter.indirect_misses.clone();

        let resolved = emitter.resolve().unwrap();
        let code = resolved.code;
        let exit_start = hot_len + resolved.external_thunk_bytes + resolved.shared_prologue_bytes;
        let interpret = exit_start + resolved.exit_trampoline_bytes + BUDGET_VENEER_BYTES;
        let dynamic_missing = interpret + INTERPRET_ONE_VENEER_BYTES;

        assert_eq!(
            relative_target(
                &code,
                misaligned.displacement_offset,
                misaligned.instruction_end,
            ),
            interpret
        );
        assert_eq!(&code[interpret..interpret + 4], &[0x49, 0x83, 0xc2, 1]);
        assert_eq!(code[interpret + 4], 0xb8);
        assert_eq!(
            u32::from_le_bytes(code[interpret + 5..interpret + 9].try_into().unwrap()),
            IMAGE_START
        );
        for miss in misses {
            assert_eq!(
                relative_target(&code, miss.displacement_offset, miss.instruction_end),
                dynamic_missing
            );
        }
        #[cfg(feature = "profile")]
        let missing_body = dynamic_missing + 4;
        #[cfg(not(feature = "profile"))]
        let missing_body = dynamic_missing;
        assert_eq!(&code[missing_body..missing_body + 2], &[0x89, 0xc8]);
        assert_eq!(code[missing_body + 2], 0xe9);
        assert_eq!(
            relative_target(&code, missing_body + 3, missing_body + 7),
            exit_start + 18
        );
    }

    #[test]
    fn empty_publication_does_not_allocate_a_dispatch_root() {
        assert!(DispatchTable::build(&[], &[]).is_none());
        assert!(LinkedProgram::publish(Vec::new(), usize::MAX).is_none());
    }

    #[test]
    fn valid_jalr_is_private_and_invalid_funct3_is_not() {
        let code = [jalr(5, 6, -1), jalr(5, 6, -1) | (1 << 12)];
        let image = image_with_code_at(&code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let valid = machine.fetch_decode(IMAGE_START).unwrap();
        let invalid = machine.fetch_decode(IMAGE_START + 4).unwrap();

        assert!(LinkedBlock::supports(valid));
        assert!(LinkedBlock::ends_block(valid));
        assert!(!LinkedBlock::supports(invalid));
    }

    #[test]
    fn compact_edges_and_cold_exits_have_exact_relocated_layout() {
        let code = [addi(5, 5, 1), addi(6, 6, 1)];
        let image = image_with_code_at(&code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let blocks = [
            block(&machine, IMAGE_START, 1),
            block(&machine, IMAGE_START + 4, 1),
        ];
        let mut emitter = Emitter::new(RegisterCache::empty());
        for block in &blocks {
            emitter
                .emit_block(&block.instructions, block.flow, block.pc)
                .unwrap();
        }
        let hot_len = emitter.code.len();
        let first_budget = emitter.budget_exits[0];
        let first_edge = emitter.edges[0];
        let missing_edge = emitter.edges[1];
        let reserved_len = blocks.iter().fold(MAX_FIXED_CODE_BYTES, |total, block| {
            total + block.reserved_code_len()
        });

        let resolved = emitter.resolve().unwrap();
        let exit_start = hot_len + resolved.external_thunk_bytes + resolved.shared_prologue_bytes;
        let actual_fixed = resolved.shared_prologue_bytes + resolved.exit_trampoline_bytes;
        let exit_bytes = resolved.exit_trampoline_bytes;
        let code = resolved.code;
        let entries = resolved.entries;

        // The first edge links natively, so its conservative ten-byte missing
        // veneer reservation is absent from the finalized image.
        assert_eq!(
            code.len() + MISSING_VENEER_BYTES + (MAX_FIXED_CODE_BYTES - actual_fixed),
            reserved_len
        );
        #[cfg(not(feature = "profile"))]
        assert_eq!(EDGE_SLOT_BYTES, 5);
        #[cfg(feature = "profile")]
        assert_eq!(EDGE_SLOT_BYTES, 9);
        assert_eq!(BUDGET_VENEER_BYTES, 14);
        assert_eq!(exit_bytes, 33);
        assert_eq!(
            &code[exit_start..exit_start + 7],
            &[0xc7, 0x47, 0x14, 3, 0, 0, 0]
        );
        assert_eq!(
            &code[exit_start + 9..exit_start + 16],
            &[0xc7, 0x47, 0x14, 2, 0, 0, 0]
        );
        assert_eq!(&code[exit_start + 16..exit_start + 18], &[0xeb, 0x07]);
        assert_eq!(
            &code[exit_start + 18..exit_start + 25],
            &[0xc7, 0x47, 0x14, 1, 0, 0, 0]
        );
        assert_eq!(
            &code[exit_start + 25..exit_start + exit_bytes],
            &[0x4c, 0x89, 0x57, 0x08, 0x89, 0x47, 0x10, 0xc3]
        );

        let first_veneer = exit_start + exit_bytes;
        assert_eq!(
            relative_target(
                &code,
                first_budget.branch.displacement_offset,
                first_budget.branch.instruction_end,
            ),
            first_veneer
        );
        assert_eq!(
            &code[first_veneer..first_veneer + 4],
            &[0x49, 0x83, 0xc2, 1]
        );
        assert_eq!(code[first_veneer + 4], 0xb8);
        assert_eq!(
            u32::from_le_bytes(code[first_veneer + 5..first_veneer + 9].try_into().unwrap()),
            IMAGE_START
        );
        assert_eq!(code[first_veneer + 9], 0xe9);
        assert_eq!(
            relative_target(&code, first_veneer + 10, first_veneer + 14),
            exit_start + 9
        );

        #[cfg(feature = "profile")]
        let direct_jump = first_edge.slot_offset + 4;
        #[cfg(not(feature = "profile"))]
        let direct_jump = first_edge.slot_offset;
        #[cfg(feature = "profile")]
        assert_eq!(
            &code[first_edge.slot_offset..direct_jump],
            &[0x48, 0xff, 0x47, super::PROFILE_DIRECT_LINKS_OFFSET as u8,]
        );
        assert_eq!(code[direct_jump], 0xe9);
        assert_eq!(
            relative_target(&code, direct_jump + 1, direct_jump + 5),
            entries[1].1.hot_offset
        );

        let missing = missing_edge.slot_offset;
        assert_eq!(code[missing], 0xe9);
        let missing_veneer = exit_start + exit_bytes + blocks.len() * BUDGET_VENEER_BYTES;
        assert_eq!(
            relative_target(&code, missing + 1, missing + 5),
            missing_veneer
        );
        assert_eq!(code[missing_veneer], 0xb8);
        assert_eq!(
            u32::from_le_bytes(
                code[missing_veneer + 1..missing_veneer + 5]
                    .try_into()
                    .unwrap()
            ),
            IMAGE_START + 8
        );
        assert_eq!(code[missing_veneer + 5], 0xe9);
        assert_eq!(
            relative_target(
                &code,
                missing_veneer + 6,
                missing_veneer + MISSING_VENEER_BYTES
            ),
            exit_start + 18
        );
        assert_eq!(EXIT_MISSING, 1);
        assert_eq!(EXIT_BUDGET, 2);
        assert_eq!(EXIT_INTERPRET_ONE, 3);
    }

    #[test]
    fn unresolved_edges_share_a_cold_veneer_by_guest_target() {
        // Offset four makes both successors name the same unavailable PC.
        let code = [branch(0, 5, 6, 4)];
        let image = image_with_code_at(&code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let block = block(&machine, IMAGE_START, 1);
        let reserved_len = LinkedProgram::fixed_code_len() + block.reserved_code_len();
        let mut emitter = Emitter::new(RegisterCache::empty());
        emitter
            .emit_block(&block.instructions, block.flow, block.pc)
            .unwrap();
        let hot_len = emitter.code.len();
        #[cfg(feature = "profile")]
        let edge_offsets = [emitter.edges[0].slot_offset, emitter.edges[1].slot_offset];
        #[cfg(not(feature = "profile"))]
        let (jump_offset, conditional) = (
            emitter.edges[0].slot_offset,
            emitter.conditional_edges[0].branch,
        );

        let resolved = emitter.resolve().unwrap();
        let exit_start = hot_len + resolved.external_thunk_bytes + resolved.shared_prologue_bytes;
        let actual_fixed = resolved.shared_prologue_bytes + resolved.exit_trampoline_bytes;
        let exit_bytes = resolved.exit_trampoline_bytes;
        let code = resolved.code;

        // Admission reserves a veneer per edge, while final relocation emits
        // one veneer for this unique unresolved guest PC.
        assert_eq!(
            code.len() + MISSING_VENEER_BYTES + (MAX_FIXED_CODE_BYTES - actual_fixed),
            reserved_len
        );
        let veneer = exit_start + exit_bytes + BUDGET_VENEER_BYTES;
        #[cfg(feature = "profile")]
        for slot in edge_offsets {
            assert_eq!(code[slot], 0xe9);
            assert_eq!(relative_target(&code, slot + 1, slot + 5), veneer);
            assert_eq!(&code[slot + 5..slot + EDGE_SLOT_BYTES], &[0x90; 4]);
        }
        #[cfg(not(feature = "profile"))]
        {
            assert_eq!(code[jump_offset], 0xe9);
            assert_eq!(
                relative_target(&code, jump_offset + 1, jump_offset + 5),
                veneer
            );
            assert_eq!(code[conditional.instruction_end - 6], 0x0f);
            assert_eq!(
                relative_target(
                    &code,
                    conditional.displacement_offset,
                    conditional.instruction_end,
                ),
                veneer
            );
        }
        assert_eq!(code[veneer], 0xb8);
        assert_eq!(
            u32::from_le_bytes(code[veneer + 1..veneer + 5].try_into().unwrap()),
            IMAGE_START + 4
        );
        assert_eq!(code[veneer + 5], 0xe9);
        assert_eq!(
            relative_target(&code, veneer + 6, veneer + MISSING_VENEER_BYTES),
            exit_start + 18
        );
    }

    #[test]
    fn checked_memory_failures_relocate_to_one_cold_refund_veneer() {
        let code = [lw(5, 6, 0)];
        let image = image_with_code_at(&code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let block = block(&machine, IMAGE_START, 1);
        let reserved_len = LinkedProgram::fixed_code_len() + block.reserved_code_len();
        let mut emitter = Emitter::new(RegisterCache::empty());
        emitter
            .emit_block(&block.instructions, block.flow, block.pc)
            .unwrap();
        let hot_len = emitter.code.len();
        let failures = emitter.interpret_one_exits[0].branches.clone();
        assert_eq!(failures.len(), 2);

        let resolved = emitter.resolve().unwrap();
        let exit_start = hot_len + resolved.external_thunk_bytes + resolved.shared_prologue_bytes;
        let actual_fixed = resolved.shared_prologue_bytes + resolved.exit_trampoline_bytes;
        let exit_bytes = resolved.exit_trampoline_bytes;
        let code = resolved.code;

        assert_eq!(
            code.len() + (MAX_FIXED_CODE_BYTES - actual_fixed),
            reserved_len
        );
        let veneer = exit_start + exit_bytes + BUDGET_VENEER_BYTES;
        for failure in failures {
            assert_eq!(
                relative_target(&code, failure.displacement_offset, failure.instruction_end),
                veneer
            );
        }
        assert_eq!(&code[veneer..veneer + 4], &[0x49, 0x83, 0xc2, 1]);
        assert_eq!(code[veneer + 4], 0xb8);
        assert_eq!(
            u32::from_le_bytes(code[veneer + 5..veneer + 9].try_into().unwrap()),
            IMAGE_START
        );
        assert_eq!(code[veneer + 9], 0xe9);
        assert_eq!(
            relative_target(&code, veneer + 10, veneer + INTERPRET_ONE_VENEER_BYTES),
            exit_start
        );
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn six_register_native_entry_restores_the_sysv_callee_saved_abi() {
        let code = (0..3)
            .flat_map(|_| (1..=6).map(|register| addi(register, register, 1)))
            .collect::<Vec<_>>();
        let image = image_with_code_at(&code, IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        let program =
            LinkedProgram::publish(vec![block(&machine, IMAGE_START, code.len())], usize::MAX)
                .unwrap();
        assert_eq!(program.cached_register_count(), 6);
        let mut registers = [0_u32; 32];
        let direct_memory = machine.memory.direct_memory();
        let mut context = super::RunContext::new(
            registers.as_mut_ptr(),
            code.len() as u64,
            IMAGE_START,
            &direct_memory,
            program.dispatch.roots_ptr(),
            program.memory.address(),
        );
        let metadata = program.entries[0];
        // SAFETY: The finalized external offset is within the live RX mapping.
        let entry = unsafe { program.memory.address().add(metadata.external_offset) };
        let mut observed = [0_u64; 6];

        // SAFETY: The probe preserves its caller's ABI, passes the exact
        // RunContext ABI to the live generated entry, and writes six outputs.
        unsafe { vm5_cache6_abi_probe(entry, &mut context, observed.as_mut_ptr()) };

        assert_eq!(
            observed,
            [
                0x1122_3344_5566_7788,
                0x8877_6655_4433_2211,
                0x0123_4567_89ab_cdef,
                0xfedc_ba98_7654_3210,
                0x0f0e_0d0c_0b0a_0908,
                0x8070_6050_4030_2010,
            ]
        );
        assert_eq!(context.remaining, 0);
        assert_eq!(context.pc, IMAGE_START + code.len() as u32 * 4);
        assert_eq!(context.exit, EXIT_MISSING);
        assert_eq!(&registers[1..=6], &[3; 6]);
    }

    #[cfg(all(
        feature = "profile",
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn cache_profile_counts_entry_exit_hits_and_every_exit_class_exactly() {
        let image = image_with_code_at(&[addi(5, 5, 1); 3], IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        let program =
            LinkedProgram::publish(vec![block(&machine, IMAGE_START, 3)], usize::MAX).unwrap();
        assert_eq!(program.cached_guest_registers()[..1], [5]);

        let mut budget_registers = [0; 32];
        budget_registers[5] = 10;
        let budget = program.entry(0).unwrap().execute(
            &mut budget_registers,
            &mut machine.memory,
            IMAGE_START,
            0,
        );
        assert_eq!(budget.stop, NativeStop::Budget);
        assert_eq!(budget_registers[5], 10);
        assert_eq!(
            (
                budget.profile.register_loads,
                budget.profile.register_stores
            ),
            (1, 1)
        );
        assert_eq!(
            (budget.profile.cache_fills, budget.profile.cache_spills),
            (1, 1)
        );
        assert_eq!(
            (
                budget.profile.cache_read_hits,
                budget.profile.cache_write_hits
            ),
            (0, 0)
        );

        let mut missing_registers = [0; 32];
        missing_registers[5] = 20;
        let missing = program.entry(0).unwrap().execute(
            &mut missing_registers,
            &mut machine.memory,
            IMAGE_START,
            3,
        );
        assert_eq!(missing.stop, NativeStop::MissingSuccessor);
        assert_eq!(missing_registers[5], 23);
        assert_eq!(
            (
                missing.profile.register_loads,
                missing.profile.register_stores
            ),
            (1, 1)
        );
        assert_eq!(
            (missing.profile.cache_fills, missing.profile.cache_spills),
            (1, 1)
        );
        assert_eq!(
            (
                missing.profile.cache_read_hits,
                missing.profile.cache_write_hits
            ),
            (3, 3)
        );

        let load_image = image_with_code_at(&[lw(5, 6, 0)], IMAGE_START);
        let mut load_machine = Machine::new(&load_image, &[], 0);
        let load_program =
            LinkedProgram::publish(vec![block(&load_machine, IMAGE_START, 1)], usize::MAX).unwrap();
        assert_eq!(load_program.cached_register_count(), 0);
        let mut trap_registers = [0; 32];
        trap_registers[5] = 99;
        trap_registers[6] = 1;
        let interpret = load_program.entry(0).unwrap().execute(
            &mut trap_registers,
            &mut load_machine.memory,
            IMAGE_START,
            1,
        );
        assert_eq!(interpret.stop, NativeStop::InterpretOne);
        assert_eq!(interpret.retired, 0);
        assert_eq!((trap_registers[5], trap_registers[6]), (99, 1));
        assert_eq!(
            (
                interpret.profile.register_loads,
                interpret.profile.register_stores
            ),
            (1, 0)
        );
        assert_eq!(
            (
                interpret.profile.cache_fills,
                interpret.profile.cache_spills
            ),
            (0, 0)
        );
        assert_eq!(
            (
                interpret.profile.cache_read_hits,
                interpret.profile.cache_write_hits,
            ),
            (0, 0)
        );

        let jalr_image = image_with_code_at(&[jalr(5, 6, 0)], IMAGE_START);
        let mut jalr_machine = Machine::new(&jalr_image, &[], 0);
        let jalr_program =
            LinkedProgram::publish(vec![block(&jalr_machine, IMAGE_START, 1)], usize::MAX).unwrap();
        assert_eq!(jalr_program.cached_register_count(), 0);
        let mut jalr_registers = [0; 32];
        jalr_registers[6] = IMAGE_START + 0x100;
        let committed = jalr_program.entry(0).unwrap().execute(
            &mut jalr_registers,
            &mut jalr_machine.memory,
            IMAGE_START,
            1,
        );
        assert_eq!(committed.stop, NativeStop::MissingSuccessor);
        assert_eq!(committed.pc, IMAGE_START + 0x100);
        assert_eq!(jalr_registers[5], IMAGE_START + 4);
        assert_eq!(
            (
                committed.profile.register_loads,
                committed.profile.register_stores
            ),
            (1, 1)
        );
        assert_eq!(
            (
                committed.profile.cache_fills,
                committed.profile.cache_spills
            ),
            (0, 0)
        );
        assert_eq!(
            (
                committed.profile.cache_read_hits,
                committed.profile.cache_write_hits
            ),
            (0, 0)
        );
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn four_flat_accesses_use_inline_entries_across_repeated_short_runs() {
        let code = [addi(5, 5, 1), addi(5, 5, 1)];
        let image = image_with_code_at(&code, IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        let program = LinkedProgram::publish(
            vec![
                block(&machine, IMAGE_START, 1),
                block(&machine, IMAGE_START + 4, 1),
            ],
            usize::MAX,
        )
        .unwrap();
        assert_eq!(program.cached_register_count(), 0);
        for entry in &program.entries {
            assert_eq!(entry.indirect_offset, entry.external_offset + 19);
            assert_eq!(entry.hot_offset, entry.indirect_offset + ENTRY_BYTES.len());
        }
        #[cfg(feature = "profile")]
        {
            assert_eq!(program.external_thunk_bytes(), 0);
            assert_eq!(program.shared_prologue_bytes(), 0);
            assert!(program.hot_code_bytes() > 0);
            assert!(program.cold_code_bytes() > 0);
        }

        for initial in [0, 10] {
            let mut registers = [0; 32];
            registers[5] = initial;
            let result = program.entry(0).unwrap().execute(
                &mut registers,
                &mut machine.memory,
                IMAGE_START,
                2,
            );

            assert_eq!(result.pc, IMAGE_START + 8);
            assert_eq!(result.retired, 2);
            assert_eq!(result.stop, NativeStop::MissingSuccessor);
            assert_eq!(registers[5], initial + 2);
            #[cfg(feature = "profile")]
            {
                assert_eq!(result.profile.blocks, 2);
                assert_eq!(result.profile.direct_links, 1);
                assert_eq!(result.profile.register_loads, 2);
                assert_eq!(result.profile.register_stores, 2);
                assert_eq!(result.profile.cache_fills, 0);
                assert_eq!(result.profile.cache_spills, 0);
                assert_eq!(result.profile.cache_read_hits, 0);
                assert_eq!(result.profile.cache_write_hits, 0);
                assert_eq!(result.profile.fallthrough_blocks, 2);
                assert_eq!(result.profile.branch_blocks, 0);
                assert_eq!(result.profile.jump_blocks, 0);
            }
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn links_backward_branch_cycles_until_the_next_block_reservation() {
        let code = [addi(5, 5, 1), beq(0, 0, -4)];
        let image = image_with_code_at(&code, IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        let program = LinkedProgram::publish(
            vec![
                block(&machine, IMAGE_START, 1),
                block(&machine, IMAGE_START + 4, 1),
            ],
            usize::MAX,
        )
        .unwrap();
        let mut registers = [0; 32];

        let result =
            program
                .entry(0)
                .unwrap()
                .execute(&mut registers, &mut machine.memory, IMAGE_START, 4);

        assert_eq!(result.pc, IMAGE_START);
        assert_eq!(result.retired, 4);
        assert_eq!(result.stop, NativeStop::Budget);
        assert_eq!(registers[5], 2);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn links_both_conditional_branch_successors() {
        let code = [beq(5, 0, 8), addi(6, 6, 1), addi(7, 7, 1)];
        let image = image_with_code_at(&code, IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        let program = LinkedProgram::publish(
            vec![
                block(&machine, IMAGE_START, 1),
                block(&machine, IMAGE_START + 4, 1),
                block(&machine, IMAGE_START + 8, 1),
            ],
            usize::MAX,
        )
        .unwrap();

        let mut taken = [0; 32];
        let taken_result =
            program
                .entry(0)
                .unwrap()
                .execute(&mut taken, &mut machine.memory, IMAGE_START, 2);
        assert_eq!(taken_result.pc, IMAGE_START + 12);
        assert_eq!(taken_result.retired, 2);
        assert_eq!(taken[6], 0);
        assert_eq!(taken[7], 1);

        let mut fallthrough = [0; 32];
        fallthrough[5] = 1;
        let fallthrough_result = program.entry(0).unwrap().execute(
            &mut fallthrough,
            &mut machine.memory,
            IMAGE_START,
            3,
        );
        assert_eq!(fallthrough_result.pc, IMAGE_START + 12);
        assert_eq!(fallthrough_result.retired, 3);
        assert_eq!(fallthrough[6], 1);
        assert_eq!(fallthrough[7], 1);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn links_jal_and_commits_the_link_register() {
        let code = [jal(5, 8), addi(6, 6, 99), addi(7, 7, 1)];
        let image = image_with_code_at(&code, IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        let program = LinkedProgram::publish(
            vec![
                block(&machine, IMAGE_START, 1),
                block(&machine, IMAGE_START + 8, 1),
            ],
            usize::MAX,
        )
        .unwrap();
        let mut registers = [0; 32];

        let result =
            program
                .entry(0)
                .unwrap()
                .execute(&mut registers, &mut machine.memory, IMAGE_START, 2);

        assert_eq!(result.pc, IMAGE_START + 12);
        assert_eq!(result.retired, 2);
        assert_eq!(registers[5], IMAGE_START + 4);
        assert_eq!(registers[6], 0);
        assert_eq!(registers[7], 1);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn short_budgets_change_no_guest_state() {
        let code = [addi(5, 5, 1), addi(5, 5, 1), addi(5, 5, 1)];
        let image = image_with_code_at(&code, IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        let program =
            LinkedProgram::publish(vec![block(&machine, IMAGE_START, 3)], usize::MAX).unwrap();

        for remaining in 0..3 {
            let mut registers = [0; 32];
            let result = program.entry(0).unwrap().execute(
                &mut registers,
                &mut machine.memory,
                IMAGE_START,
                remaining,
            );
            assert_eq!(result.pc, IMAGE_START);
            assert_eq!(result.retired, 0);
            assert_eq!(result.stop, NativeStop::Budget);
            assert_eq!(registers, [0; 32]);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn maximum_block_budget_is_reserved_as_an_unsigned_count() {
        let code = vec![addi(5, 5, 1); 64];
        let image = image_with_code_at(&code, IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        let program =
            LinkedProgram::publish(vec![block(&machine, IMAGE_START, 64)], usize::MAX).unwrap();

        let mut short = [0; 32];
        let short_result =
            program
                .entry(0)
                .unwrap()
                .execute(&mut short, &mut machine.memory, IMAGE_START, 63);
        assert_eq!(short_result.pc, IMAGE_START);
        assert_eq!(short_result.retired, 0);
        assert_eq!(short_result.stop, NativeStop::Budget);
        assert_eq!(short[5], 0);

        let mut exact = [0; 32];
        let exact_result =
            program
                .entry(0)
                .unwrap()
                .execute(&mut exact, &mut machine.memory, IMAGE_START, 64);
        assert_eq!(exact_result.pc, IMAGE_START + 64 * 4);
        assert_eq!(exact_result.retired, 64);
        assert_eq!(exact_result.stop, NativeStop::MissingSuccessor);
        assert_eq!(exact[5], 64);

        let mut huge = [0; 32];
        let huge_result = program.entry(0).unwrap().execute(
            &mut huge,
            &mut machine.memory,
            IMAGE_START,
            u64::MAX,
        );
        assert_eq!(huge_result.pc, IMAGE_START + 64 * 4);
        assert_eq!(huge_result.retired, 64);
        assert_eq!(huge_result.stop, NativeStop::MissingSuccessor);
        assert_eq!(huge[5], 64);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn failed_successor_reservation_preserves_the_committed_prefix_budget() {
        let code = [addi(5, 5, 1), addi(6, 6, 1), addi(6, 6, 1), addi(6, 6, 1)];
        let image = image_with_code_at(&code, IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        let program = LinkedProgram::publish(
            vec![
                block(&machine, IMAGE_START, 1),
                block(&machine, IMAGE_START + 4, 3),
            ],
            usize::MAX,
        )
        .unwrap();
        let mut registers = [0; 32];

        let result =
            program
                .entry(0)
                .unwrap()
                .execute(&mut registers, &mut machine.memory, IMAGE_START, 2);

        assert_eq!(result.pc, IMAGE_START + 4);
        assert_eq!(result.retired, 1);
        assert_eq!(result.stop, NativeStop::Budget);
        assert_eq!(registers[5], 1);
        assert_eq!(registers[6], 0);
        #[cfg(feature = "profile")]
        {
            assert_eq!(result.profile.blocks, 1);
            assert_eq!(result.profile.direct_links, 1);
            assert_eq!(result.profile.fallthrough_blocks, 1);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn repeated_invocations_have_run_local_budget_and_register_state() {
        let code = [addi(5, 5, 1), beq(0, 0, -4)];
        let image = image_with_code_at(&code, IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        let program =
            LinkedProgram::publish(vec![block(&machine, IMAGE_START, 2)], usize::MAX).unwrap();

        for expected in [2, 3] {
            let mut registers = [0; 32];
            registers[5] = expected - 1;
            let result = program.entry(0).unwrap().execute(
                &mut registers,
                &mut machine.memory,
                IMAGE_START,
                2,
            );
            assert_eq!(result.retired, 2);
            assert_eq!(registers[5], expected);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn publication_obeys_code_budget_and_drop_unmaps_rx_memory() {
        let code = [addi(5, 5, 1)];
        let image = image_with_code_at(&code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        assert!(LinkedProgram::publish(vec![block(&machine, IMAGE_START, 1)], 4_095).is_none());
        let program = LinkedProgram::publish(vec![block(&machine, IMAGE_START, 1)], 4_096).unwrap();
        let address = program.memory.address() as usize;
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        let line = maps
            .lines()
            .find(|line| {
                let range = line.split_whitespace().next().unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let start = usize::from_str_radix(start, 16).unwrap();
                let end = usize::from_str_radix(end, 16).unwrap();
                start <= address && address < end
            })
            .unwrap();
        let permissions = line.split_whitespace().nth(1).unwrap();
        assert!(permissions.starts_with("r-x"), "{permissions}");
        assert!(!permissions.contains('w'), "{permissions}");

        let unmap_status = program.memory.unmap_status();
        drop(program);
        assert_eq!(
            unmap_status.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "LinkedProgram::drop failed to unmap its executable memory"
        );
    }
}
