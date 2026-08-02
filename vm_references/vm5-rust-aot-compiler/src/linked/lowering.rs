//! RV32IM decoding and mapping-independent block lowering.

use rv32vm_rust_common::machine::DecodedInstruction;
use rv32vm_rust_x86_block_compiler::BlockInstruction;

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
    pub(super) const fn successors(self) -> [Option<u32>; 2] {
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
    pub(super) pc: u32,
    pub(super) instructions: Vec<Lowering>,
    pub(super) flow: BlockFlow,
    pub(super) reserved_code_len: usize,
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

/// Decodes the supported, contiguous prefix without applying any x86 sizing
/// or image-admission policy.
pub(super) fn lower_block(
    instructions: &[BlockInstruction],
) -> Option<(u32, Vec<Lowering>, BlockFlow)> {
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
    Some((pc, lowered, flow))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Lowering {
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
pub(super) enum MemoryWidth {
    Byte,
    Half,
    Word,
}

impl MemoryWidth {
    pub(super) const fn bytes(self) -> u32 {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ImmediateOperation {
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
pub(super) enum RegisterOperation {
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
