//! Emits the x86-64 instruction subset shared by the native VMs.

use rv32vm_rust_common::machine::DecodedInstruction;

use crate::{
    BlockInstruction,
    lowering::{BranchCondition, ImmediateOperation, Lowering, RegisterOperation},
};

enum Flow {
    Continue,
    Return,
}

/// Final machine-code bytes and their committed guest-instruction count.
pub struct CompiledBlock {
    pub(crate) code: Vec<u8>,
    pub(crate) instruction_count: usize,
}

impl CompiledBlock {
    /// Emits one supported native block without publishing executable memory.
    pub fn compile(instructions: &[BlockInstruction]) -> Option<Self> {
        compile(instructions)
    }

    pub fn code_len(&self) -> usize {
        self.code.len()
    }

    pub const fn instruction_count(&self) -> usize {
        self.instruction_count
    }
}

pub(super) fn compile(instructions: &[BlockInstruction]) -> Option<CompiledBlock> {
    let mut emitter = Emitter::new();
    let mut next_pc = instructions.first()?.as_ref().ok()?.pc();
    let mut instruction_count = 0;
    let mut returned = false;

    for instruction in instructions {
        let Ok(instruction) = *instruction else {
            break;
        };
        if instruction.pc() != next_pc {
            break;
        }
        let Some(flow) = emitter.instruction(instruction) else {
            break;
        };
        match flow {
            Flow::Continue => {
                instruction_count += 1;
                next_pc = next_pc.wrapping_add(4);
            }
            Flow::Return => {
                instruction_count += 1;
                returned = true;
                break;
            }
        }
    }

    if instruction_count == 0 {
        return None;
    }
    if !returned {
        emitter.return_pc(next_pc);
    }
    Some(CompiledBlock {
        code: emitter.code,
        instruction_count,
    })
}

struct Emitter {
    code: Vec<u8>,
}

impl Emitter {
    fn new() -> Self {
        Self {
            // `endbr64` permits indirect entry on hosts enforcing CET.
            code: vec![0xf3, 0x0f, 0x1e, 0xfa],
        }
    }

    fn instruction(&mut self, instruction: DecodedInstruction) -> Option<Flow> {
        let lowering = Lowering::decode(instruction)?;
        Some(match lowering {
            Lowering::WriteImmediate { destination, value } => {
                self.write_immediate(destination, value);
                Flow::Continue
            }
            Lowering::Jump {
                destination,
                link,
                target,
            } => {
                self.write_immediate(destination, link);
                self.return_pc(target);
                Flow::Return
            }
            Lowering::Branch {
                left,
                right,
                condition,
                fallthrough,
                target,
            } => self.branch(left, right, condition, fallthrough, target),
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
            Lowering::Fence => Flow::Continue,
        })
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
        destination: usize,
        left: usize,
        right: usize,
        operation: RegisterOperation,
    ) -> Flow {
        if destination == 0 {
            return Flow::Continue;
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
        Flow::Continue
    }

    fn branch(
        &mut self,
        left: usize,
        right: usize,
        condition: BranchCondition,
        fallthrough: u32,
        target: u32,
    ) -> Flow {
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
        self.code.extend_from_slice(&[0x39, 0xc8, 0x0f, condition]);
        self.code.extend_from_slice(&6_i32.to_le_bytes());
        self.return_pc(fallthrough);
        self.return_pc(target);
        Flow::Return
    }

    fn write_immediate(&mut self, register: usize, value: u32) {
        if register != 0 {
            self.mov_eax(value);
            self.store_eax(register);
        }
    }

    fn load_eax(&mut self, register: usize) {
        self.code
            .extend_from_slice(&[0x8b, 0x47, register_offset(register)]);
    }

    fn load_ecx(&mut self, register: usize) {
        self.code
            .extend_from_slice(&[0x8b, 0x4f, register_offset(register)]);
    }

    fn store_eax(&mut self, register: usize) {
        self.code
            .extend_from_slice(&[0x89, 0x47, register_offset(register)]);
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

    fn return_pc(&mut self, pc: u32) {
        self.mov_eax(pc);
        self.code.push(0xc3);
    }
}

const fn register_offset(register: usize) -> u8 {
    (register * size_of::<u32>()) as u8
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::memory::IMAGE_START;

    use super::compile;
    use crate::test_support::{addi, decoded_block, lw, machine_with_code};

    #[test]
    fn compiles_supported_prefixes_only() {
        let machine = machine_with_code(&[addi(5, 0, 1), addi(5, 5, 1), lw(6, 0, 0)], IMAGE_START);
        let block = decoded_block(&machine, IMAGE_START);

        let compiled = compile(&block).unwrap();

        assert_eq!(compiled.instruction_count, 2);
        assert_eq!(compiled.code.last(), Some(&0xc3));
    }

    #[test]
    fn compiles_single_supported_instructions() {
        let machine = machine_with_code(&[addi(5, 0, 1), lw(6, 0, 0)], IMAGE_START);
        let block = decoded_block(&machine, IMAGE_START);

        assert_eq!(compile(&block).unwrap().instruction_count, 1);
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
}
