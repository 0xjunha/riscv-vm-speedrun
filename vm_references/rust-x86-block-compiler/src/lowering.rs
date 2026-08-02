//! Decodes the RV32IM instructions supported by the x86-64 emitter.

use rv32vm_rust_common::machine::DecodedInstruction;

/// A supported RV32IM instruction prepared for x86-64 emission.
#[derive(Clone, Copy)]
pub(crate) enum Lowering {
    WriteImmediate {
        destination: usize,
        value: u32,
    },
    Jump {
        destination: usize,
        link: u32,
        target: u32,
    },
    JumpRegister {
        destination: usize,
        source: usize,
        offset: u32,
        link: u32,
    },
    Branch {
        left: usize,
        right: usize,
        condition: BranchCondition,
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
        destination: usize,
        source: usize,
        offset: u32,
        width: MemoryWidth,
        signed: bool,
    },
    Store {
        source: usize,
        base: usize,
        offset: u32,
        width: MemoryWidth,
    },
    Fence,
}

impl Lowering {
    pub(crate) fn decode(instruction: DecodedInstruction) -> Option<Self> {
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
                Some(Self::Jump {
                    destination: instruction.rd(),
                    link: instruction.pc().wrapping_add(4),
                    target,
                })
            }
            0x67 if instruction.funct3() == 0 => Some(Self::JumpRegister {
                destination: instruction.rd(),
                source: instruction.rs1(),
                offset: sign_extend(instruction.raw() >> 20, 12),
                link: instruction.pc().wrapping_add(4),
            }),
            0x63 => {
                let condition = match instruction.funct3() {
                    0 => BranchCondition::Equal,
                    1 => BranchCondition::NotEqual,
                    4 => BranchCondition::LessThan,
                    5 => BranchCondition::GreaterOrEqual,
                    6 => BranchCondition::Below,
                    7 => BranchCondition::AboveOrEqual,
                    _ => return None,
                };
                let target = instruction.branch_target();
                Some(Self::Branch {
                    left: instruction.rs1(),
                    right: instruction.rs2(),
                    condition,
                    fallthrough: instruction.pc().wrapping_add(4),
                    target,
                })
            }
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
                    destination: instruction.rd(),
                    source: instruction.rs1(),
                    offset: sign_extend(instruction.raw() >> 20, 12),
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
                    source: instruction.rs2(),
                    base: instruction.rs1(),
                    offset: sign_extend(encoded, 12),
                    width,
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

    pub(crate) const fn register_usage(self) -> RegisterUsage {
        match self {
            Self::WriteImmediate { destination, .. } | Self::Jump { destination, .. } => {
                RegisterUsage::write(destination)
            }
            Self::JumpRegister {
                destination,
                source,
                ..
            } => RegisterUsage::read_write(source, destination),
            Self::Branch { left, right, .. } => RegisterUsage::read_two(left, right),
            Self::Immediate { destination: 0, .. } => RegisterUsage::none(),
            Self::Immediate {
                destination,
                source,
                ..
            } => RegisterUsage::read_write(source, destination),
            Self::Load {
                destination: 0,
                source,
                ..
            } => RegisterUsage::read(source),
            Self::Load {
                destination,
                source,
                ..
            } => RegisterUsage::read_write(source, destination),
            Self::Register {
                destination,
                left: _,
                right: _,
                operation,
            } if destination == 0
                && !matches!(
                    operation,
                    RegisterOperation::Divide
                        | RegisterOperation::DivideUnsigned
                        | RegisterOperation::Remainder
                        | RegisterOperation::RemainderUnsigned
                ) =>
            {
                RegisterUsage::none()
            }
            Self::Register {
                destination,
                left,
                right,
                ..
            } => RegisterUsage::read_two_write(left, right, destination),
            Self::Store { source, base, .. } => RegisterUsage::read_two(base, source),
            Self::Fence => RegisterUsage::none(),
        }
    }

    pub(crate) const fn ends_native_block(self) -> bool {
        matches!(
            self,
            Self::Jump { .. } | Self::JumpRegister { .. } | Self::Branch { .. }
        )
    }

    pub(crate) const fn uses_r9_scratch(self) -> bool {
        matches!(
            self,
            Self::Register {
                operation: RegisterOperation::MultiplyHighSignedUnsigned,
                ..
            }
        )
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RegisterUsage {
    pub(crate) reads: [Option<usize>; 2],
    pub(crate) write: Option<usize>,
}

impl RegisterUsage {
    const fn none() -> Self {
        Self {
            reads: [None, None],
            write: None,
        }
    }

    const fn write(destination: usize) -> Self {
        Self {
            reads: [None, None],
            write: Some(destination),
        }
    }

    const fn read_write(source: usize, destination: usize) -> Self {
        Self {
            reads: [Some(source), None],
            write: Some(destination),
        }
    }

    const fn read(source: usize) -> Self {
        Self {
            reads: [Some(source), None],
            write: None,
        }
    }

    const fn read_two(left: usize, right: usize) -> Self {
        Self {
            reads: [Some(left), Some(right)],
            write: None,
        }
    }

    const fn read_two_write(left: usize, right: usize, destination: usize) -> Self {
        Self {
            reads: [Some(left), Some(right)],
            write: Some(destination),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum BranchCondition {
    Equal,
    NotEqual,
    LessThan,
    GreaterOrEqual,
    Below,
    AboveOrEqual,
}

#[derive(Clone, Copy)]
pub(crate) enum ImmediateOperation {
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

#[derive(Clone, Copy)]
pub(crate) enum RegisterOperation {
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
    MultiplyHigh,
    MultiplyHighSignedUnsigned,
    MultiplyHighUnsigned,
    Divide,
    DivideUnsigned,
    Remainder,
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
            (1, 1) => Some(Self::MultiplyHigh),
            (1, 2) => Some(Self::MultiplyHighSignedUnsigned),
            (1, 3) => Some(Self::MultiplyHighUnsigned),
            (1, 4) => Some(Self::Divide),
            (1, 5) => Some(Self::DivideUnsigned),
            (1, 6) => Some(Self::Remainder),
            (1, 7) => Some(Self::RemainderUnsigned),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum MemoryWidth {
    Byte = 1,
    Half = 2,
    Word = 4,
}

impl MemoryWidth {
    pub(crate) const fn bytes(self) -> u32 {
        self as u32
    }
}

const fn sign_extend(value: u32, bits: u32) -> u32 {
    ((value << (32 - bits)) as i32 >> (32 - bits)) as u32
}
