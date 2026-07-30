//! Emits the small x86-64 instruction subset used by this JIT.

use rv32vm_rust_common::machine::DecodedInstruction;

use crate::block::BasicBlock;

const MIN_NATIVE_INSTRUCTIONS: usize = 2;

enum Flow {
    Continue,
    Return,
}

/// Final machine-code bytes and their committed guest-instruction count.
pub(super) struct CompiledBlock {
    pub(super) code: Vec<u8>,
    pub(super) instruction_count: usize,
}

pub(super) fn compile(block: &BasicBlock) -> Option<CompiledBlock> {
    let mut emitter = Emitter::new();
    let mut next_pc = block.instructions().first()?.as_ref().ok()?.pc();
    let mut instruction_count = 0;
    let mut returned = false;

    for instruction in block.instructions() {
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

    if instruction_count < MIN_NATIVE_INSTRUCTIONS {
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
        match instruction.opcode() {
            0x37 => {
                self.write_immediate(instruction.rd(), instruction.raw() & 0xffff_f000);
                Some(Flow::Continue)
            }
            0x17 => {
                self.write_immediate(
                    instruction.rd(),
                    instruction
                        .pc()
                        .wrapping_add(instruction.raw() & 0xffff_f000),
                );
                Some(Flow::Continue)
            }
            0x6f => self.jal(instruction),
            0x63 => self.branch(instruction),
            0x13 => self.immediate(instruction),
            0x33 => self.register(instruction),
            0x0f if instruction.funct3() == 0 => Some(Flow::Continue),
            _ => None,
        }
    }

    fn immediate(&mut self, instruction: DecodedInstruction) -> Option<Flow> {
        let operation = match (instruction.funct3(), instruction.funct7()) {
            (0, _) => Immediate::Add(sign_extend(instruction.raw() >> 20, 12)),
            (2, _) => Immediate::SetLessThan(sign_extend(instruction.raw() >> 20, 12)),
            (3, _) => Immediate::SetBelow(sign_extend(instruction.raw() >> 20, 12)),
            (4, _) => Immediate::Xor(sign_extend(instruction.raw() >> 20, 12)),
            (6, _) => Immediate::Or(sign_extend(instruction.raw() >> 20, 12)),
            (7, _) => Immediate::And(sign_extend(instruction.raw() >> 20, 12)),
            (1, 0) => Immediate::ShiftLeft(instruction.rs2() as u8),
            (5, 0) => Immediate::ShiftRight(instruction.rs2() as u8),
            (5, 0x20) => Immediate::ShiftRightArithmetic(instruction.rs2() as u8),
            _ => return None,
        };
        if instruction.rd() == 0 {
            return Some(Flow::Continue);
        }

        self.load_eax(instruction.rs1());
        match operation {
            Immediate::Add(value) => self.eax_immediate(0x05, value),
            Immediate::Xor(value) => self.eax_immediate(0x35, value),
            Immediate::Or(value) => self.eax_immediate(0x0d, value),
            Immediate::And(value) => self.eax_immediate(0x25, value),
            Immediate::ShiftLeft(count) => self.eax_shift(0xe0, count),
            Immediate::ShiftRight(count) => self.eax_shift(0xe8, count),
            Immediate::ShiftRightArithmetic(count) => self.eax_shift(0xf8, count),
            Immediate::SetLessThan(value) => {
                self.mov_ecx(value);
                self.compare_and_set(0x9c);
            }
            Immediate::SetBelow(value) => {
                self.mov_ecx(value);
                self.compare_and_set(0x92);
            }
        }
        self.store_eax(instruction.rd());
        Some(Flow::Continue)
    }

    fn register(&mut self, instruction: DecodedInstruction) -> Option<Flow> {
        let operation = match (instruction.funct7(), instruction.funct3()) {
            (0, 0) => Register::Add,
            (0x20, 0) => Register::Subtract,
            (0, 1) => Register::ShiftLeft,
            (0, 2) => Register::SetLessThan,
            (0, 3) => Register::SetBelow,
            (0, 4) => Register::Xor,
            (0, 5) => Register::ShiftRight,
            (0x20, 5) => Register::ShiftRightArithmetic,
            (0, 6) => Register::Or,
            (0, 7) => Register::And,
            (1, 0) => Register::Multiply,
            _ => return None,
        };
        if instruction.rd() == 0 {
            return Some(Flow::Continue);
        }

        self.load_eax(instruction.rs1());
        self.load_ecx(instruction.rs2());
        match operation {
            Register::Add => self.code.extend_from_slice(&[0x01, 0xc8]),
            Register::Subtract => self.code.extend_from_slice(&[0x29, 0xc8]),
            Register::Xor => self.code.extend_from_slice(&[0x31, 0xc8]),
            Register::Or => self.code.extend_from_slice(&[0x09, 0xc8]),
            Register::And => self.code.extend_from_slice(&[0x21, 0xc8]),
            Register::Multiply => self.code.extend_from_slice(&[0x0f, 0xaf, 0xc1]),
            Register::ShiftLeft => self.code.extend_from_slice(&[0xd3, 0xe0]),
            Register::ShiftRight => self.code.extend_from_slice(&[0xd3, 0xe8]),
            Register::ShiftRightArithmetic => self.code.extend_from_slice(&[0xd3, 0xf8]),
            Register::SetLessThan => self.compare_and_set(0x9c),
            Register::SetBelow => self.compare_and_set(0x92),
        }
        self.store_eax(instruction.rd());
        Some(Flow::Continue)
    }

    fn branch(&mut self, instruction: DecodedInstruction) -> Option<Flow> {
        let condition = match instruction.funct3() {
            0 => 0x84,
            1 => 0x85,
            4 => 0x8c,
            5 => 0x8d,
            6 => 0x82,
            7 => 0x83,
            _ => return None,
        };
        let raw = instruction.raw();
        let encoded = (((raw >> 31) & 1) << 12)
            | (((raw >> 7) & 1) << 11)
            | (((raw >> 25) & 0x3f) << 5)
            | (((raw >> 8) & 0xf) << 1);
        let target = instruction.pc().wrapping_add(sign_extend(encoded, 13));
        if target & 3 != 0 {
            return None;
        }

        self.load_eax(instruction.rs1());
        self.load_ecx(instruction.rs2());
        self.code.extend_from_slice(&[0x39, 0xc8, 0x0f, condition]);
        self.code.extend_from_slice(&6_i32.to_le_bytes());
        self.return_pc(instruction.pc().wrapping_add(4));
        self.return_pc(target);
        Some(Flow::Return)
    }

    fn jal(&mut self, instruction: DecodedInstruction) -> Option<Flow> {
        let raw = instruction.raw();
        let encoded = ((raw >> 31) << 20)
            | (((raw >> 12) & 0xff) << 12)
            | (((raw >> 20) & 1) << 11)
            | (((raw >> 21) & 0x3ff) << 1);
        let target = instruction.pc().wrapping_add(sign_extend(encoded, 21));
        if target & 3 != 0 {
            return None;
        }
        self.write_immediate(instruction.rd(), instruction.pc().wrapping_add(4));
        self.return_pc(target);
        Some(Flow::Return)
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

enum Immediate {
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

enum Register {
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

const fn register_offset(register: usize) -> u8 {
    (register * size_of::<u32>()) as u8
}

const fn sign_extend(value: u32, bits: u32) -> u32 {
    ((value << (32 - bits)) as i32 >> (32 - bits)) as u32
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::memory::IMAGE_START;

    use super::compile;
    use crate::{
        block::BasicBlock,
        test_support::{addi, lw, machine_with_code_at},
    };

    #[test]
    fn compiles_supported_prefixes_only() {
        let machine =
            machine_with_code_at(&[addi(5, 0, 1), addi(5, 5, 1), lw(6, 0, 0)], IMAGE_START);
        let block = BasicBlock::translate(&machine, IMAGE_START);

        let compiled = compile(&block).unwrap();

        assert_eq!(compiled.instruction_count, 2);
        assert_eq!(compiled.code.last(), Some(&0xc3));
    }

    #[test]
    fn rejects_prefixes_too_short_to_amortize_a_native_call() {
        let machine = machine_with_code_at(&[addi(5, 0, 1), lw(6, 0, 0)], IMAGE_START);
        let block = BasicBlock::translate(&machine, IMAGE_START);

        assert!(compile(&block).is_none());
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
            let machine = machine_with_code_at(&[instruction, addi(0, 0, 0)], IMAGE_START);
            let block = BasicBlock::translate(&machine, IMAGE_START);
            assert!(compile(&block).is_none());
        }
    }
}
