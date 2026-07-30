//! Decodes the RV32IM instructions supported by the x86-64 emitter.

use rv32vm_rust_common::machine::DecodedInstruction;

/// A supported RV32IM instruction prepared for x86-64 emission.
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
                target.is_multiple_of(4).then_some(Self::Jump {
                    destination: instruction.rd(),
                    link: instruction.pc().wrapping_add(4),
                    target,
                })
            }
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
}

pub(crate) enum BranchCondition {
    Equal,
    NotEqual,
    LessThan,
    GreaterOrEqual,
    Below,
    AboveOrEqual,
}

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
