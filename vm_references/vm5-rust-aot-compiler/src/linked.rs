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
const EXIT_TRAMPOLINE_BYTES: usize = 33;
const EXIT_MISSING: u32 = 1;
const EXIT_BUDGET: u32 = 2;
const EXIT_INTERPRET_ONE: u32 = 3;
const _: () = assert!(PAGE_SIZE.is_power_of_two());
const _: () = assert!(PAGE_SIZE == 1_usize << PAGE_SHIFT);
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
const PROFILE_FALLTHROUGH_OFFSET: usize = 104;
#[cfg(feature = "profile")]
const PROFILE_BRANCH_OFFSET: usize = 112;
#[cfg(feature = "profile")]
const PROFILE_JUMP_OFFSET: usize = 120;
#[cfg(feature = "profile")]
const PROFILE_MEMORY_LOADS_OFFSET: usize = 128;
#[cfg(feature = "profile")]
const PROFILE_MEMORY_STORES_OFFSET: usize = 136;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockFlow {
    Fallthrough {
        pc: u32,
    },
    /// A checked memory terminator owns both its fast successor and precise
    /// one-instruction slow exit.
    CheckedFallthrough {
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
            Self::Fallthrough { pc } | Self::CheckedFallthrough { pc } | Self::Jump { pc } => {
                [Some(pc), None]
            }
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

    /// Reports whether this supported instruction must terminate a native
    /// block. Checked memory operations do so to keep one precise interpreter
    /// retry point for all slow and trapping cases.
    pub(crate) fn ends_block(instruction: DecodedInstruction) -> bool {
        Lowering::decode(instruction)
            .and_then(|lowering| lowering.flow(instruction.pc().wrapping_add(4)))
            .is_some()
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

    pub(crate) const fn successors(&self) -> [Option<u32>; 2] {
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
            Self::Load { pc, .. } | Self::Store { pc, .. } => Some(BlockFlow::CheckedFallthrough {
                pc: pc.wrapping_add(4),
            }),
            _ => {
                let _ = next_pc;
                None
            }
        }
    }

    #[cfg(feature = "profile")]
    fn register_traffic(self) -> (usize, usize) {
        match self {
            Self::WriteImmediate { destination, .. } | Self::Jump { destination, .. } => {
                (0, usize::from(destination != 0))
            }
            Self::Branch { .. } => (2, 0),
            Self::Immediate { destination, .. } => {
                (usize::from(destination != 0), usize::from(destination != 0))
            }
            Self::Register { destination, .. } => (
                usize::from(destination != 0) * 2,
                usize::from(destination != 0),
            ),
            // Checked memory traffic is counted at its exact dynamic point:
            // attempted source reads happen before validation, while a load's
            // destination write happens only after the fast path succeeds.
            Self::Load { .. } | Self::Store { .. } | Self::IndirectJump { .. } => (0, 0),
            Self::Fence => (0, 0),
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
    let mut emitter = Emitter::new();
    emitter.emit_block(instructions, flow, 0)?;
    emitter.reserved_code_len()
}

#[derive(Clone, Copy)]
struct EntryMetadata {
    external_offset: usize,
    indirect_offset: usize,
    hot_offset: usize,
}

type ResolvedImage = (Vec<u8>, Vec<(u32, EntryMetadata)>);

#[derive(Clone, Copy)]
struct EdgeRelocation {
    slot_offset: usize,
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LocalFixup {
    displacement_offset: usize,
    instruction_end: usize,
}

struct Emitter {
    code: Vec<u8>,
    entries: Vec<(u32, EntryMetadata)>,
    edges: Vec<EdgeRelocation>,
    budget_exits: Vec<BudgetRelocation>,
    interpret_one_exits: Vec<InterpretOneRelocation>,
    indirect_misses: Vec<LocalFixup>,
    local_fixups: Vec<LocalFixup>,
}

impl Emitter {
    fn new() -> Self {
        Self {
            code: Vec::new(),
            entries: Vec::new(),
            edges: Vec::new(),
            budget_exits: Vec::new(),
            interpret_one_exits: Vec::new(),
            indirect_misses: Vec::new(),
            local_fixups: Vec::new(),
        }
    }

    fn reserved_code_len(&self) -> Option<usize> {
        self.code
            .len()
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
            .checked_add(self.edges.len().checked_mul(MISSING_VENEER_BYTES)?)
    }

    fn emit_block(&mut self, instructions: &[Lowering], flow: BlockFlow, pc: u32) -> Option<()> {
        let external_offset = self.code.len();
        self.code.extend_from_slice(&ENTRY_BYTES);
        // External Rust dispatch reloads the private ABI. In-image indirect
        // dispatch instead lands at the second ENDBR64 below, retaining the
        // live register, memory, and R10 budget anchors. Direct edges skip both
        // entry pads and enter at `hot_offset`.
        self.code.extend_from_slice(&[0x48, 0x8b, 0x37]); // mov rsi, [rdi]
        self.code.extend_from_slice(&[0x4c, 0x8b, 0x47, 0x18]); // mov r8, [rdi+24]
        self.code.extend_from_slice(&[0x4c, 0x8b, 0x4f, 0x20]); // mov r9, [rdi+32]
        self.code.extend_from_slice(&[0x4c, 0x8b, 0x57, 0x08]); // mov r10, [rdi+8]
        let indirect_offset = self.code.len();
        self.code.extend_from_slice(&ENTRY_BYTES);
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
        #[cfg(feature = "profile")]
        self.profile_block(instructions, flow)?;

        for instruction in instructions {
            match *instruction {
                Lowering::WriteImmediate { destination, value } => {
                    self.write_immediate(destination, value);
                }
                Lowering::Jump {
                    destination,
                    link,
                    target,
                } => {
                    self.write_immediate(destination, link);
                    self.edge_slot(target)?;
                }
                Lowering::IndirectJump {
                    pc,
                    destination,
                    source,
                    immediate,
                    link,
                } => self.indirect_jump(pc, destination, source, immediate, link)?,
                Lowering::Branch {
                    left,
                    right,
                    condition,
                    fallthrough,
                    target,
                } => {
                    self.load_eax(left);
                    self.load_ecx(right);
                    self.code
                        .extend_from_slice(&[0x39, 0xc8, 0x0f, condition.x86()]);
                    self.code
                        .extend_from_slice(&(EDGE_SLOT_BYTES as i32).to_le_bytes());
                    self.edge_slot(fallthrough)?;
                    self.edge_slot(target)?;
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
                    destination,
                    base,
                    immediate,
                    width,
                    signed,
                    ..
                } => {
                    let BlockFlow::CheckedFallthrough { pc: successor } = flow else {
                        return None;
                    };
                    self.checked_load(successor, destination, base, immediate, width, signed)?;
                }
                Lowering::Store {
                    base,
                    source,
                    immediate,
                    width,
                    ..
                } => {
                    let BlockFlow::CheckedFallthrough { pc: successor } = flow else {
                        return None;
                    };
                    self.checked_store(successor, base, source, immediate, width)?;
                }
                Lowering::Fence => {}
            }
        }

        if matches!(flow, BlockFlow::Fallthrough { .. }) {
            let [Some(pc), None] = flow.successors() else {
                return None;
            };
            self.edge_slot(pc)?;
        }
        Some(())
    }

    #[cfg(feature = "profile")]
    fn profile_block(&mut self, instructions: &[Lowering], flow: BlockFlow) -> Option<()> {
        let (loads, stores) =
            instructions
                .iter()
                .try_fold((0_usize, 0_usize), |(loads, stores), instruction| {
                    let traffic = instruction.register_traffic();
                    Some((
                        loads.checked_add(traffic.0)?,
                        stores.checked_add(traffic.1)?,
                    ))
                })?;
        self.increment_context(PROFILE_BLOCKS_OFFSET);
        self.add_context(PROFILE_REGISTER_LOADS_OFFSET, loads)?;
        self.add_context(PROFILE_REGISTER_STORES_OFFSET, stores)?;
        match flow {
            BlockFlow::Fallthrough { .. } => self.increment_context(PROFILE_FALLTHROUGH_OFFSET),
            BlockFlow::CheckedFallthrough { .. } => {}
            BlockFlow::Branch { .. } => self.increment_context(PROFILE_BRANCH_OFFSET),
            BlockFlow::Jump { .. } => self.increment_context(PROFILE_JUMP_OFFSET),
            BlockFlow::IndirectJump { .. } => {}
        }
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

    fn immediate(&mut self, destination: usize, source: usize, operation: ImmediateOperation) {
        if destination == 0 {
            return;
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
        self.load_eax(left);
        self.load_ecx(right);
        match operation {
            RegisterOperation::Add => self.code.extend_from_slice(&[0x01, 0xc8]),
            RegisterOperation::Subtract => self.code.extend_from_slice(&[0x29, 0xc8]),
            RegisterOperation::Xor => self.code.extend_from_slice(&[0x31, 0xc8]),
            RegisterOperation::Or => self.code.extend_from_slice(&[0x09, 0xc8]),
            RegisterOperation::And => self.code.extend_from_slice(&[0x21, 0xc8]),
            RegisterOperation::Multiply => self.code.extend_from_slice(&[0x0f, 0xaf, 0xc1]),
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
        #[cfg(feature = "profile")]
        self.increment_context(PROFILE_REGISTER_LOADS_OFFSET);
        if immediate != 0 {
            self.eax_immediate(0x05, immediate);
        }
        self.code.extend_from_slice(&[0x83, 0xe0, 0xfe]); // and eax, -2
        self.code.extend_from_slice(&[0xa8, 0x02]); // test al, 2
        let misaligned = self.cold_jcc(0x85)?; // jnz precise slow path
        self.code.extend_from_slice(&[0x89, 0xc1]); // mov ecx, eax

        if destination != 0 {
            self.store_immediate(destination, link);
            #[cfg(feature = "profile")]
            self.increment_context(PROFILE_REGISTER_STORES_OFFSET);
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
        self.interpret_one_exit(vec![misaligned], pc)
    }

    fn checked_load(
        &mut self,
        successor: u32,
        destination: usize,
        base: usize,
        immediate: u32,
        width: MemoryWidth,
        signed: bool,
    ) -> Option<()> {
        let pc = successor.wrapping_sub(4);
        let failures = self.checked_memory_address(base, immediate, width, PERM_READ, None)?;

        // A readable sparse page has the architecturally defined value zero.
        self.code.extend_from_slice(&[0x48, 0x85, 0xd2]); // test rdx, rdx
        let sparse = self.local_jcc(0x84)?; // jz sparse
        self.mask_page_offset()?;
        match (width, signed) {
            (MemoryWidth::Byte, true) => {
                self.code.extend_from_slice(&[0x0f, 0xbe, 0x04, 0x02]);
            }
            (MemoryWidth::Byte, false) => {
                self.code.extend_from_slice(&[0x0f, 0xb6, 0x04, 0x02]);
            }
            (MemoryWidth::Half, true) => {
                self.code.extend_from_slice(&[0x0f, 0xbf, 0x04, 0x02]);
            }
            (MemoryWidth::Half, false) => {
                self.code.extend_from_slice(&[0x0f, 0xb7, 0x04, 0x02]);
            }
            (MemoryWidth::Word, _) => {
                self.code.extend_from_slice(&[0x8b, 0x04, 0x02]);
            }
        }
        let loaded = self.local_jump()?;
        self.bind_local(sparse)?;
        self.code.extend_from_slice(&[0x31, 0xc0]); // xor eax, eax
        self.bind_local(loaded)?;
        if destination != 0 {
            self.store_eax(destination);
            #[cfg(feature = "profile")]
            self.increment_context(PROFILE_REGISTER_STORES_OFFSET);
        }
        #[cfg(feature = "profile")]
        {
            self.increment_context(PROFILE_MEMORY_LOADS_OFFSET);
            self.increment_context(PROFILE_FALLTHROUGH_OFFSET);
        }
        self.edge_slot(successor)?;
        self.interpret_one_exit(failures, pc)
    }

    fn checked_store(
        &mut self,
        successor: u32,
        base: usize,
        source: usize,
        immediate: u32,
        width: MemoryWidth,
    ) -> Option<()> {
        let pc = successor.wrapping_sub(4);
        let mut failures =
            self.checked_memory_address(base, immediate, width, PERM_WRITE, Some(source))?;
        self.code.extend_from_slice(&[0x48, 0x85, 0xd2]); // test rdx, rdx
        failures.push(self.cold_jcc(0x84)?); // sparse stores allocate in Rust
        self.mask_page_offset()?;
        match width {
            MemoryWidth::Byte => self.code.extend_from_slice(&[0x88, 0x0c, 0x02]),
            MemoryWidth::Half => self.code.extend_from_slice(&[0x66, 0x89, 0x0c, 0x02]),
            MemoryWidth::Word => self.code.extend_from_slice(&[0x89, 0x0c, 0x02]),
        }
        #[cfg(feature = "profile")]
        {
            self.increment_context(PROFILE_MEMORY_STORES_OFFSET);
            self.increment_context(PROFILE_FALLTHROUGH_OFFSET);
        }
        self.edge_slot(successor)?;
        self.interpret_one_exit(failures, pc)
    }

    /// Computes EAX = wrapping guest address and RDX = resident page base.
    /// Every returned fixup targets the caller's single precise slow exit.
    fn checked_memory_address(
        &mut self,
        base: usize,
        immediate: u32,
        width: MemoryWidth,
        permission: u8,
        store_source: Option<usize>,
    ) -> Option<Vec<LocalFixup>> {
        self.load_eax(base);
        if let Some(source) = store_source {
            self.load_ecx(source);
        }
        #[cfg(feature = "profile")]
        self.add_context(
            PROFILE_REGISTER_LOADS_OFFSET,
            if store_source.is_some() { 2 } else { 1 },
        )?;
        if immediate != 0 {
            self.eax_immediate(0x05, immediate);
        }

        let mut failures = Vec::with_capacity(3);
        let alignment_mask = width.bytes() - 1;
        if alignment_mask != 0 {
            self.code.extend_from_slice(&[0xa8, alignment_mask as u8]); // test al, mask
            failures.push(self.cold_jcc(0x85)?); // jnz slow
        }

        self.code.push(0x3d); // cmp eax, last valid start address
        self.code
            .extend_from_slice(&(ADDRESS_SPACE_SIZE - width.bytes()).to_le_bytes());
        failures.push(self.cold_jcc(0x87)?); // ja slow

        self.code.extend_from_slice(&[0x89, 0xc2]); // mov edx, eax
        let page_shift = u8::try_from(PAGE_SHIFT).ok()?;
        self.code.extend_from_slice(&[0xc1, 0xea, page_shift]); // shr edx, PAGE_SHIFT
        self.code
            .extend_from_slice(&[0x41, 0xf6, 0x04, 0x10, permission]); // test [r8+rdx], perm
        failures.push(self.cold_jcc(0x84)?); // jz slow
        self.code.extend_from_slice(&[0x49, 0x8b, 0x14, 0xd1]); // mov rdx, [r9+rdx*8]
        Some(failures)
    }

    fn mask_page_offset(&mut self) -> Option<()> {
        let page_mask = u32::try_from(PAGE_SIZE.checked_sub(1)?).ok()?;
        self.code.push(0x25); // and eax, PAGE_SIZE - 1
        self.code.extend_from_slice(&page_mask.to_le_bytes());
        Some(())
    }

    fn interpret_one_exit(&mut self, branches: Vec<LocalFixup>, pc: u32) -> Option<()> {
        (!branches.is_empty()).then(|| {
            self.interpret_one_exits
                .push(InterpretOneRelocation { branches, pc });
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
            self.mov_eax(value);
            self.store_eax(register);
        }
    }

    fn store_immediate(&mut self, register: usize, value: u32) {
        self.code
            .extend_from_slice(&[0xc7, 0x46, register_offset(register)]);
        self.code.extend_from_slice(&value.to_le_bytes());
    }

    fn load_eax(&mut self, register: usize) {
        self.code
            .extend_from_slice(&[0x8b, 0x46, register_offset(register)]);
    }

    fn load_ecx(&mut self, register: usize) {
        self.code
            .extend_from_slice(&[0x8b, 0x4e, register_offset(register)]);
    }

    fn store_eax(&mut self, register: usize) {
        self.code
            .extend_from_slice(&[0x89, 0x46, register_offset(register)]);
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

    fn eax_shift(&mut self, extension: u8, count: u8) {
        self.code.extend_from_slice(&[0xc1, extension, count]);
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

    fn cold_jump(&mut self) -> Option<LocalFixup> {
        let displacement_offset = self.code.len().checked_add(1)?;
        let instruction_end = self.code.len().checked_add(5)?;
        self.code.extend_from_slice(&[0xe9, 0, 0, 0, 0]);
        Some(LocalFixup {
            displacement_offset,
            instruction_end,
        })
    }

    fn emit_exit_trampolines(&mut self) -> Option<ExitTargets> {
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
        self.code.extend_from_slice(&[0x4c, 0x89, 0x57, 0x08]); // mov [rdi+8], r10
        self.code.extend_from_slice(&[0x89, 0x47, 0x10]); // mov [rdi+16], eax
        self.code.push(0xc3);
        (self.code.len().checked_sub(start)? == EXIT_TRAMPOLINE_BYTES).then_some(ExitTargets {
            missing,
            budget,
            interpret_one,
        })
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
        // The terminal memory instruction has not committed. Refund exactly
        // that instruction from the block reservation before Rust retries it.
        self.code.extend_from_slice(&[0x49, 0x83, 0xc2, 0x01]); // add r10, 1
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
        let mut hot_by_pc = BTreeMap::new();
        for &(pc, entry) in &self.entries {
            if hot_by_pc.insert(pc, entry.hot_offset).is_some() {
                return None;
            }
        }

        if self.entries.is_empty() {
            return self.code.is_empty().then_some((self.code, Vec::new()));
        }

        let exit_targets = self.emit_exit_trampolines()?;
        for relocation in std::mem::take(&mut self.budget_exits) {
            self.emit_budget_veneer(relocation, exit_targets.budget)?;
        }
        for relocation in std::mem::take(&mut self.interpret_one_exits) {
            self.emit_interpret_one_veneer(relocation, exit_targets.interpret_one)?;
        }
        let indirect_misses = std::mem::take(&mut self.indirect_misses);
        self.emit_indirect_missing_veneer(indirect_misses, exit_targets.missing)?;
        let mut missing_by_pc = BTreeMap::new();
        for edge in self.edges.clone() {
            if let Some(&target) = hot_by_pc.get(&edge.target_pc) {
                patch_edge(&mut self.code, edge.slot_offset, target)?;
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
        Some((self.code, self.entries))
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

fn patch_edge(code: &mut [u8], slot_offset: usize, target_offset: usize) -> Option<()> {
    #[cfg(feature = "profile")]
    let jump_offset = slot_offset.checked_add(4)?;
    #[cfg(not(feature = "profile"))]
    let jump_offset = slot_offset;
    let instruction_end = jump_offset.checked_add(5)?;
    let displacement = i64::try_from(target_offset).ok()? - i64::try_from(instruction_end).ok()?;
    let displacement = i32::try_from(displacement).ok()?;
    let slot = code.get_mut(slot_offset..slot_offset.checked_add(EDGE_SLOT_BYTES)?)?;
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
    page_addresses: *const usize,
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
    fallthrough_blocks: u64,
    #[cfg(feature = "profile")]
    branch_blocks: u64,
    #[cfg(feature = "profile")]
    jump_blocks: u64,
    #[cfg(feature = "profile")]
    memory_loads: u64,
    #[cfg(feature = "profile")]
    memory_stores: u64,
}

const _: () = assert!(std::mem::offset_of!(RunContext, registers) == 0);
const _: () = assert!(std::mem::offset_of!(RunContext, remaining) == 8);
const _: () = assert!(std::mem::offset_of!(RunContext, pc) == 16);
const _: () = assert!(std::mem::offset_of!(RunContext, exit) == 20);
const _: () = assert!(std::mem::offset_of!(RunContext, permissions) == 24);
const _: () = assert!(std::mem::offset_of!(RunContext, page_addresses) == 32);
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
const _: () = assert!(std::mem::offset_of!(RunContext, fallthrough_blocks) == 104);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, branch_blocks) == 112);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, jump_blocks) == 120);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, memory_loads) == 128);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, memory_stores) == 136);

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
    pub(crate) fallthrough_blocks: u64,
    pub(crate) branch_blocks: u64,
    pub(crate) jump_blocks: u64,
    pub(crate) memory_loads: u64,
    pub(crate) memory_stores: u64,
}

/// Owns one fully relocated VM5 linked image.
pub(crate) struct LinkedProgram {
    memory: ExecutableMemory,
    entries: Vec<EntryMetadata>,
    dispatch: DispatchTable,
}

impl LinkedProgram {
    pub(crate) const fn fixed_code_len() -> usize {
        EXIT_TRAMPOLINE_BYTES
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
        let mut emitter = Emitter::new();
        for block in &blocks {
            if emitter
                .emit_block(&block.instructions, block.flow, block.pc)
                .is_none()
            {
                return (None, 0);
            }
        }
        let Some((code, resolved_entries)) = emitter.resolve() else {
            return (None, 0);
        };
        let code_len = code.len();
        if code_len > reserved_len || code_len > code_budget {
            return (None, code_len);
        }
        let Some(dispatch) = DispatchTable::build(&code, &resolved_entries) else {
            return (None, code_len);
        };
        let entries = resolved_entries
            .into_iter()
            .map(|(_, metadata)| metadata)
            .collect();
        let program = ExecutableMemory::publish(&code, code_budget).map(|memory| Self {
            memory,
            entries,
            dispatch,
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
        let mut context = RunContext {
            registers: registers.as_mut_ptr(),
            remaining,
            pc,
            exit: 0,
            permissions: direct_memory.permissions_ptr(),
            page_addresses: direct_memory.page_addresses_ptr(),
            dispatch_pages: self.program.dispatch.roots_ptr(),
            code_base: self.program.memory.address(),
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
            fallthrough_blocks: 0,
            #[cfg(feature = "profile")]
            branch_blocks: 0,
            #[cfg(feature = "profile")]
            jump_blocks: 0,
            #[cfg(feature = "profile")]
            memory_loads: 0,
            #[cfg(feature = "profile")]
            memory_stores: 0,
        };
        // SAFETY: The mapping is RX and live, context/register borrows are
        // exclusive for the synchronous call, and emitted code uses only
        // SysV caller-saved registers without touching the host stack.
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
                fallthrough_blocks: context.fallthrough_blocks,
                branch_blocks: context.branch_blocks,
                jump_blocks: context.jump_blocks,
                memory_loads: context.memory_loads,
                memory_stores: context.memory_stores,
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
    use rv32vm_rust_common::{machine::Machine, memory::IMAGE_START};
    use rv32vm_rust_x86_block_compiler::BlockInstruction;

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use super::NativeStop;
    use super::{
        BUDGET_VENEER_BYTES, DispatchTable, EDGE_SLOT_BYTES, ENTRY_BYTES, EXIT_BUDGET,
        EXIT_INTERPRET_ONE, EXIT_MISSING, EXIT_TRAMPOLINE_BYTES, Emitter,
        INTERPRET_ONE_VENEER_BYTES, LinkedBlock, LinkedProgram, MISSING_VENEER_BYTES,
        mapping_length,
    };
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use crate::test_support::beq;
    use crate::test_support::{addi, image_with_code_at, jal, jalr, lw};

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
        assert_eq!(result.profile.register_loads, 16);
        assert_eq!(result.profile.register_stores, 8);
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

    #[test]
    fn every_external_and_indirect_entry_is_a_cet_landing_pad() {
        let code = [addi(5, 5, 1), addi(6, 6, 1)];
        let image = image_with_code_at(&code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let blocks = [
            block(&machine, IMAGE_START, 1),
            block(&machine, IMAGE_START + 4, 1),
        ];
        let mut emitter = super::Emitter::new();
        for block in &blocks {
            emitter
                .emit_block(&block.instructions, block.flow, block.pc)
                .unwrap();
        }
        for (_, entry) in &emitter.entries {
            assert_eq!(
                &emitter.code[entry.external_offset..entry.external_offset + ENTRY_BYTES.len()],
                &ENTRY_BYTES
            );
            assert_eq!(
                &emitter.code[entry.indirect_offset - 4..entry.indirect_offset],
                &[0x4c, 0x8b, 0x57, 0x08]
            );
            assert_eq!(
                &emitter.code[entry.indirect_offset..entry.hot_offset],
                &ENTRY_BYTES
            );
        }
    }

    #[test]
    fn dispatch_table_encodes_internal_pads_while_direct_edges_target_hot_code() {
        let code = [addi(5, 5, 1), addi(6, 6, 1)];
        let image = image_with_code_at(&code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let blocks = [
            block(&machine, IMAGE_START, 1),
            block(&machine, IMAGE_START + 4, 1),
        ];
        let mut emitter = Emitter::new();
        for block in &blocks {
            emitter
                .emit_block(&block.instructions, block.flow, block.pc)
                .unwrap();
        }
        let first_edge = emitter.edges[0];
        let (code, entries) = emitter.resolve().unwrap();
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

        #[cfg(feature = "profile")]
        let jump_offset = first_edge.slot_offset + 4;
        #[cfg(not(feature = "profile"))]
        let jump_offset = first_edge.slot_offset;
        assert_eq!(code[jump_offset], 0xe9);
        assert_eq!(
            relative_target(&code, jump_offset + 1, jump_offset + 5),
            entries[1].1.hot_offset
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
        let mut emitter = Emitter::new();

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
        let mut emitter = Emitter::new();
        emitter
            .emit_block(&block.instructions, block.flow, block.pc)
            .unwrap();
        let hot_len = emitter.code.len();
        let misaligned = emitter.interpret_one_exits[0].branches[0];
        let misses = emitter.indirect_misses.clone();

        let (code, _) = emitter.resolve().unwrap();
        let interpret = hot_len + EXIT_TRAMPOLINE_BYTES + BUDGET_VENEER_BYTES;
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
            hot_len + 18
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
        let mut emitter = Emitter::new();
        for block in &blocks {
            emitter
                .emit_block(&block.instructions, block.flow, block.pc)
                .unwrap();
        }
        let hot_len = emitter.code.len();
        let first_budget = emitter.budget_exits[0];
        let first_edge = emitter.edges[0];
        let missing_edge = emitter.edges[1];
        let reserved_len = blocks.iter().fold(EXIT_TRAMPOLINE_BYTES, |total, block| {
            total + block.reserved_code_len()
        });

        let (code, entries) = emitter.resolve().unwrap();

        // The first edge links natively, so its conservative ten-byte missing
        // veneer reservation is absent from the finalized image.
        assert_eq!(code.len() + MISSING_VENEER_BYTES, reserved_len);
        #[cfg(not(feature = "profile"))]
        assert_eq!(EDGE_SLOT_BYTES, 5);
        #[cfg(feature = "profile")]
        assert_eq!(EDGE_SLOT_BYTES, 9);
        assert_eq!(BUDGET_VENEER_BYTES, 14);
        assert_eq!(EXIT_TRAMPOLINE_BYTES, 33);
        assert_eq!(&code[hot_len..hot_len + 7], &[0xc7, 0x47, 0x14, 3, 0, 0, 0]);
        assert_eq!(
            &code[hot_len + 9..hot_len + 16],
            &[0xc7, 0x47, 0x14, 2, 0, 0, 0]
        );
        assert_eq!(&code[hot_len + 16..hot_len + 18], &[0xeb, 0x07]);
        assert_eq!(
            &code[hot_len + 18..hot_len + 25],
            &[0xc7, 0x47, 0x14, 1, 0, 0, 0]
        );
        assert_eq!(
            &code[hot_len + 25..hot_len + EXIT_TRAMPOLINE_BYTES],
            &[0x4c, 0x89, 0x57, 0x08, 0x89, 0x47, 0x10, 0xc3]
        );

        let first_veneer = hot_len + EXIT_TRAMPOLINE_BYTES;
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
            hot_len + 9
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
        let missing_veneer = hot_len + EXIT_TRAMPOLINE_BYTES + blocks.len() * BUDGET_VENEER_BYTES;
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
            hot_len + 18
        );
        assert_eq!(EXIT_MISSING, 1);
        assert_eq!(EXIT_BUDGET, 2);
        assert_eq!(EXIT_INTERPRET_ONE, 3);
    }

    #[test]
    fn unresolved_edges_share_a_cold_veneer_by_guest_target() {
        // Offset four makes both successors name the same unavailable PC.
        let code = [branch(0, 0, 0, 4)];
        let image = image_with_code_at(&code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let block = block(&machine, IMAGE_START, 1);
        let reserved_len = LinkedProgram::fixed_code_len() + block.reserved_code_len();
        let mut emitter = Emitter::new();
        emitter
            .emit_block(&block.instructions, block.flow, block.pc)
            .unwrap();
        let hot_len = emitter.code.len();
        let edge_offsets = [emitter.edges[0].slot_offset, emitter.edges[1].slot_offset];

        let (code, _) = emitter.resolve().unwrap();

        // Admission reserves a veneer per edge, while final relocation emits
        // one veneer for this unique unresolved guest PC.
        assert_eq!(code.len() + MISSING_VENEER_BYTES, reserved_len);
        let veneer = hot_len + EXIT_TRAMPOLINE_BYTES + BUDGET_VENEER_BYTES;
        for slot in edge_offsets {
            assert_eq!(code[slot], 0xe9);
            assert_eq!(relative_target(&code, slot + 1, slot + 5), veneer);
            #[cfg(feature = "profile")]
            assert_eq!(&code[slot + 5..slot + EDGE_SLOT_BYTES], &[0x90; 4]);
        }
        assert_eq!(code[veneer], 0xb8);
        assert_eq!(
            u32::from_le_bytes(code[veneer + 1..veneer + 5].try_into().unwrap()),
            IMAGE_START + 4
        );
        assert_eq!(code[veneer + 5], 0xe9);
        assert_eq!(
            relative_target(&code, veneer + 6, veneer + MISSING_VENEER_BYTES),
            hot_len + 18
        );
    }

    #[test]
    fn checked_memory_failures_relocate_to_one_cold_refund_veneer() {
        let code = [lw(5, 6, 0)];
        let image = image_with_code_at(&code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let block = block(&machine, IMAGE_START, 1);
        let reserved_len = LinkedProgram::fixed_code_len() + block.reserved_code_len();
        let mut emitter = Emitter::new();
        emitter
            .emit_block(&block.instructions, block.flow, block.pc)
            .unwrap();
        let hot_len = emitter.code.len();
        let failures = emitter.interpret_one_exits[0].branches.clone();

        let (code, _) = emitter.resolve().unwrap();

        assert_eq!(code.len(), reserved_len);
        let veneer = hot_len + EXIT_TRAMPOLINE_BYTES + BUDGET_VENEER_BYTES;
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
            hot_len
        );
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn links_forward_fallthrough_and_returns_missing_successor() {
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
        let mut registers = [0; 32];

        let result =
            program
                .entry(0)
                .unwrap()
                .execute(&mut registers, &mut machine.memory, IMAGE_START, 2);

        assert_eq!(result.pc, IMAGE_START + 8);
        assert_eq!(result.retired, 2);
        assert_eq!(result.stop, NativeStop::MissingSuccessor);
        assert_eq!(registers[5], 2);
        #[cfg(feature = "profile")]
        {
            assert_eq!(result.profile.blocks, 2);
            assert_eq!(result.profile.direct_links, 1);
            assert_eq!(result.profile.register_loads, 2);
            assert_eq!(result.profile.register_stores, 2);
            assert_eq!(result.profile.fallthrough_blocks, 2);
            assert_eq!(result.profile.branch_blocks, 0);
            assert_eq!(result.profile.jump_blocks, 0);
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
