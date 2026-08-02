//! VM5-private whole-image native linking.
//!
//! The shared block compiler deliberately keeps VM4's one-block call ABI.
//! VM5 reproduces that compiler's non-trapping lowering here so it can add a
//! per-block budget check and turn known exits into direct native edges.

use std::collections::BTreeMap;

use rv32vm_rust_common::machine::DecodedInstruction;
use rv32vm_rust_x86_block_compiler::BlockInstruction;

const ENTRY_BYTES: [u8; 4] = [0xf3, 0x0f, 0x1e, 0xfa];
const EDGE_SLOT_BYTES: usize = 16;
const EXIT_MISSING: u32 = 1;
const EXIT_BUDGET: u32 = 2;
#[cfg(feature = "profile")]
const PROFILE_BLOCKS_OFFSET: u8 = 24;
#[cfg(feature = "profile")]
const PROFILE_DIRECT_LINKS_OFFSET: u8 = 32;
#[cfg(feature = "profile")]
const PROFILE_REGISTER_LOADS_OFFSET: u8 = 40;
#[cfg(feature = "profile")]
const PROFILE_REGISTER_STORES_OFFSET: u8 = 48;
#[cfg(feature = "profile")]
const PROFILE_FALLTHROUGH_OFFSET: u8 = 56;
#[cfg(feature = "profile")]
const PROFILE_BRANCH_OFFSET: u8 = 64;
#[cfg(feature = "profile")]
const PROFILE_JUMP_OFFSET: u8 = 72;

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
        }
    }
}

/// A bounded native block staged during LOAD, before image-wide relocation.
pub(crate) struct LinkedBlock {
    pc: u32,
    instructions: Vec<Lowering>,
    flow: BlockFlow,
    code_len: usize,
}

impl LinkedBlock {
    /// Reports whether VM5's private linked backend can lower an instruction.
    ///
    /// AOT discovery uses this predicate so candidate boundaries cannot drift
    /// when the separately versioned VM4 block compiler gains a lowering.
    pub(crate) fn supports(instruction: DecodedInstruction) -> bool {
        Lowering::decode(instruction).is_some()
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
            let terminal = lowering.flow(next_pc);
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
        let code_len = emitted_len(&lowered, flow)?;
        Some(Self {
            pc,
            instructions: lowered,
            flow,
            code_len,
        })
    }

    pub(crate) fn instruction_count(&self) -> usize {
        self.instructions.len()
    }

    pub(crate) const fn code_len(&self) -> usize {
        self.code_len
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
            Self::Fence => (0, 0),
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
            _ => None,
        }
    }
}

const fn sign_extend(value: u32, bits: u32) -> u32 {
    ((value << (32 - bits)) as i32 >> (32 - bits)) as u32
}

fn emitted_len(instructions: &[Lowering], flow: BlockFlow) -> Option<usize> {
    let mut emitter = Emitter::new();
    emitter.emit_block(instructions, flow, 0)?;
    Some(emitter.code.len())
}

#[derive(Clone, Copy)]
struct EntryMetadata {
    external_offset: usize,
    hot_offset: usize,
}

#[derive(Clone, Copy)]
struct EdgeRelocation {
    slot_offset: usize,
    target_pc: u32,
}

struct Emitter {
    code: Vec<u8>,
    entries: Vec<(u32, EntryMetadata)>,
    edges: Vec<EdgeRelocation>,
}

impl Emitter {
    fn new() -> Self {
        Self {
            code: Vec::new(),
            entries: Vec::new(),
            edges: Vec::new(),
        }
    }

    fn emit_block(&mut self, instructions: &[Lowering], flow: BlockFlow, pc: u32) -> Option<()> {
        let external_offset = self.code.len();
        self.code.extend_from_slice(&ENTRY_BYTES);
        // The external entry is the only indirect host target. Direct guest
        // edges enter at `hot_offset` and retain RSI as the register-file base.
        self.code.extend_from_slice(&[0x48, 0x8b, 0x37]); // mov rsi, [rdi]
        let hot_offset = self.code.len();
        self.entries.push((
            pc,
            EntryMetadata {
                external_offset,
                hot_offset,
            },
        ));

        let count = u8::try_from(instructions.len()).ok()?;
        if count == 0 || count > 64 {
            return None;
        }
        // Reserve the entire non-trapping block before any guest-visible
        // effect. A short block is returned untouched for one-step fallback.
        self.code
            .extend_from_slice(&[0x48, 0x83, 0x7f, 0x08, count]); // cmp [rdi+8], count
        self.code.extend_from_slice(&[0x73, EDGE_SLOT_BYTES as u8]); // jae reserved
        self.exit_slot(pc, EXIT_BUDGET)?;
        self.code
            .extend_from_slice(&[0x48, 0x83, 0x6f, 0x08, count]); // sub [rdi+8], count
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
                } => self.register(destination, left, right, operation),
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
        self.increment_context(match flow {
            BlockFlow::Fallthrough { .. } => PROFILE_FALLTHROUGH_OFFSET,
            BlockFlow::Branch { .. } => PROFILE_BRANCH_OFFSET,
            BlockFlow::Jump { .. } => PROFILE_JUMP_OFFSET,
        });
        Some(())
    }

    #[cfg(feature = "profile")]
    fn increment_context(&mut self, offset: u8) {
        self.code.extend_from_slice(&[0x48, 0xff, 0x47, offset]);
    }

    #[cfg(feature = "profile")]
    fn add_context(&mut self, offset: u8, value: usize) -> Option<()> {
        if value == 0 {
            return Some(());
        }
        self.code.extend_from_slice(&[0x48, 0x81, 0x47, offset]);
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
    ) {
        if destination == 0 {
            return;
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
            RegisterOperation::ShiftLeft => self.code.extend_from_slice(&[0xd3, 0xe0]),
            RegisterOperation::ShiftRight => self.code.extend_from_slice(&[0xd3, 0xe8]),
            RegisterOperation::ShiftRightArithmetic => {
                self.code.extend_from_slice(&[0xd3, 0xf8]);
            }
            RegisterOperation::SetLessThan => self.compare_and_set(0x9c),
            RegisterOperation::SetBelow => self.compare_and_set(0x92),
        }
        self.store_eax(destination);
    }

    fn write_immediate(&mut self, register: usize, value: u32) {
        if register != 0 {
            self.mov_eax(value);
            self.store_eax(register);
        }
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

    fn edge_slot(&mut self, pc: u32) -> Option<()> {
        let slot_offset = self.code.len();
        self.exit_slot(pc, EXIT_MISSING)?;
        self.edges.push(EdgeRelocation {
            slot_offset,
            target_pc: pc,
        });
        Some(())
    }

    fn exit_slot(&mut self, pc: u32, reason: u32) -> Option<()> {
        let start = self.code.len();
        self.code.extend_from_slice(&[0xc7, 0x47, 0x10]);
        self.code.extend_from_slice(&pc.to_le_bytes());
        self.code.extend_from_slice(&[0xc7, 0x47, 0x14]);
        self.code.extend_from_slice(&reason.to_le_bytes());
        self.code.push(0xc3);
        let end = start.checked_add(EDGE_SLOT_BYTES)?;
        self.code.resize(end, 0x90);
        Some(())
    }

    fn resolve(mut self) -> Option<(Vec<u8>, Vec<EntryMetadata>)> {
        let mut hot_by_pc = BTreeMap::new();
        for &(pc, entry) in &self.entries {
            if hot_by_pc.insert(pc, entry.hot_offset).is_some() {
                return None;
            }
        }

        for edge in &self.edges {
            let Some(&target) = hot_by_pc.get(&edge.target_pc) else {
                continue;
            };
            patch_edge(&mut self.code, edge.slot_offset, target)?;
        }
        Some((
            self.code,
            self.entries.into_iter().map(|(_, entry)| entry).collect(),
        ))
    }
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
        slot[..4].copy_from_slice(&[0x48, 0xff, 0x47, PROFILE_DIRECT_LINKS_OFFSET]);
    }
    let jump = jump_offset - slot_offset;
    slot[jump] = 0xe9;
    slot[jump + 1..jump + 5].copy_from_slice(&displacement.to_le_bytes());
    Some(())
}

const fn register_offset(register: usize) -> u8 {
    (register * size_of::<u32>()) as u8
}

#[repr(C)]
struct RunContext {
    registers: *mut u32,
    remaining: u64,
    pc: u32,
    exit: u32,
    #[cfg(feature = "profile")]
    blocks: u64,
    #[cfg(feature = "profile")]
    direct_links: u64,
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
}

const _: () = assert!(std::mem::offset_of!(RunContext, registers) == 0);
const _: () = assert!(std::mem::offset_of!(RunContext, remaining) == 8);
const _: () = assert!(std::mem::offset_of!(RunContext, pc) == 16);
const _: () = assert!(std::mem::offset_of!(RunContext, exit) == 20);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, blocks) == 24);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, direct_links) == 32);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, register_loads) == 40);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, register_stores) == 48);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, fallthrough_blocks) == 56);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, branch_blocks) == 64);
#[cfg(feature = "profile")]
const _: () = assert!(std::mem::offset_of!(RunContext, jump_blocks) == 72);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeStop {
    MissingSuccessor,
    Budget,
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
    pub(crate) register_loads: u64,
    pub(crate) register_stores: u64,
    pub(crate) fallthrough_blocks: u64,
    pub(crate) branch_blocks: u64,
    pub(crate) jump_blocks: u64,
}

/// Owns one fully relocated VM5 linked image.
pub(crate) struct LinkedProgram {
    memory: ExecutableMemory,
    entries: Vec<EntryMetadata>,
}

impl LinkedProgram {
    pub(crate) fn publish(blocks: Vec<LinkedBlock>, code_budget: usize) -> Option<Self> {
        let mut emitter = Emitter::new();
        for block in &blocks {
            emitter.emit_block(&block.instructions, block.flow, block.pc)?;
        }
        let (code, entries) = emitter.resolve()?;
        if code.len() > code_budget {
            return None;
        }
        let memory = ExecutableMemory::publish(&code, code_budget)?;
        Some(Self { memory, entries })
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
}

#[derive(Clone, Copy)]
pub(crate) struct LinkedEntry<'a> {
    program: &'a LinkedProgram,
    metadata: EntryMetadata,
}

impl LinkedEntry<'_> {
    pub(crate) fn execute(self, registers: &mut [u32; 32], pc: u32, remaining: u64) -> NativeRun {
        self.execute_inner(registers, pc, remaining)
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    fn execute_inner(self, registers: &mut [u32; 32], pc: u32, remaining: u64) -> NativeRun {
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
            #[cfg(feature = "profile")]
            blocks: 0,
            #[cfg(feature = "profile")]
            direct_links: 0,
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
        };
        // SAFETY: The mapping is RX and live, context/register borrows are
        // exclusive for the synchronous call, and emitted code uses only
        // SysV caller-saved registers without touching the host stack.
        unsafe { entry(&mut context) };
        debug_assert!(context.remaining <= remaining);
        let stop = match context.exit {
            EXIT_MISSING => NativeStop::MissingSuccessor,
            EXIT_BUDGET => NativeStop::Budget,
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
                register_loads: context.register_loads,
                register_stores: context.register_stores,
                fallthrough_blocks: context.fallthrough_blocks,
                branch_blocks: context.branch_blocks,
                jump_blocks: context.jump_blocks,
            },
        }
    }

    #[cfg(not(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    )))]
    fn execute_inner(self, _registers: &mut [u32; 32], _pc: u32, _remaining: u64) -> NativeRun {
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

    use super::{ENTRY_BYTES, LinkedBlock, mapping_length};
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use super::{LinkedProgram, NativeStop};
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use crate::test_support::beq;
    use crate::test_support::{addi, image_with_code_at, jal, lw};

    fn decoded(machine: &Machine, start: u32, count: usize) -> Vec<BlockInstruction> {
        (0..count)
            .map(|index| machine.fetch_decode(start + index as u32 * 4))
            .collect()
    }

    fn block(machine: &Machine, start: u32, count: usize) -> LinkedBlock {
        LinkedBlock::compile(&decoded(machine, start, count)).unwrap()
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
        for &(register, value) in registers {
            expected.registers[register] = value;
        }
        let staged = block(&expected, IMAGE_START, 1);
        let program = LinkedProgram::publish(vec![staged], usize::MAX).unwrap();
        let mut actual_registers = expected.registers;

        let decoded = expected.fetch_decode(IMAGE_START);
        assert!(expected.execute_one(decoded).is_none());
        let actual = program
            .entry(0)
            .unwrap()
            .execute(&mut actual_registers, IMAGE_START, 1);

        assert_eq!(actual.retired, 1);
        assert_eq!(actual.pc, expected.pc);
        assert_eq!(actual_registers, expected.registers);
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
    fn every_external_entry_is_cet_landing_pad() {
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
        }
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
        let machine = Machine::new(&image, &[], 0);
        let program = LinkedProgram::publish(
            vec![
                block(&machine, IMAGE_START, 1),
                block(&machine, IMAGE_START + 4, 1),
            ],
            usize::MAX,
        )
        .unwrap();
        let mut registers = [0; 32];

        let result = program
            .entry(0)
            .unwrap()
            .execute(&mut registers, IMAGE_START, 2);

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
        let machine = Machine::new(&image, &[], 0);
        let program = LinkedProgram::publish(
            vec![
                block(&machine, IMAGE_START, 1),
                block(&machine, IMAGE_START + 4, 1),
            ],
            usize::MAX,
        )
        .unwrap();
        let mut registers = [0; 32];

        let result = program
            .entry(0)
            .unwrap()
            .execute(&mut registers, IMAGE_START, 4);

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
        let machine = Machine::new(&image, &[], 0);
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
        let taken_result = program
            .entry(0)
            .unwrap()
            .execute(&mut taken, IMAGE_START, 2);
        assert_eq!(taken_result.pc, IMAGE_START + 12);
        assert_eq!(taken_result.retired, 2);
        assert_eq!(taken[6], 0);
        assert_eq!(taken[7], 1);

        let mut fallthrough = [0; 32];
        fallthrough[5] = 1;
        let fallthrough_result =
            program
                .entry(0)
                .unwrap()
                .execute(&mut fallthrough, IMAGE_START, 3);
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
        let machine = Machine::new(&image, &[], 0);
        let program = LinkedProgram::publish(
            vec![
                block(&machine, IMAGE_START, 1),
                block(&machine, IMAGE_START + 8, 1),
            ],
            usize::MAX,
        )
        .unwrap();
        let mut registers = [0; 32];

        let result = program
            .entry(0)
            .unwrap()
            .execute(&mut registers, IMAGE_START, 2);

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
        let machine = Machine::new(&image, &[], 0);
        let program =
            LinkedProgram::publish(vec![block(&machine, IMAGE_START, 3)], usize::MAX).unwrap();

        for remaining in 0..3 {
            let mut registers = [0; 32];
            let result = program
                .entry(0)
                .unwrap()
                .execute(&mut registers, IMAGE_START, remaining);
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
    fn repeated_invocations_have_run_local_budget_and_register_state() {
        let code = [addi(5, 5, 1), beq(0, 0, -4)];
        let image = image_with_code_at(&code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let program =
            LinkedProgram::publish(vec![block(&machine, IMAGE_START, 2)], usize::MAX).unwrap();

        for expected in [2, 3] {
            let mut registers = [0; 32];
            registers[5] = expected - 1;
            let result = program
                .entry(0)
                .unwrap()
                .execute(&mut registers, IMAGE_START, 2);
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
