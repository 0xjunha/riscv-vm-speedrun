//! Owns native-code publication and invocation for x86-64 Linux.

mod memory;

use std::mem;

use crate::{block::BasicBlock, native::emitter};

use self::memory::ExecutableMemory;

type Entry = unsafe extern "C" fn(*mut u32) -> u32;

/// Owns one executable native block and its guest-instruction count.
pub(crate) struct NativeBlock {
    memory: ExecutableMemory,
    entry: Entry,
    instruction_count: usize,
}

impl NativeBlock {
    pub(crate) fn compile(block: &BasicBlock, code_budget: usize) -> Option<Self> {
        let compiled = emitter::compile(block)?;
        let memory = ExecutableMemory::publish(&compiled.code, code_budget)?;
        debug_assert_eq!(size_of::<Entry>(), size_of::<*const u8>());
        // SAFETY: `memory` contains finalized bytes emitted for `Entry`, is RX,
        // and remains owned by the returned block for the entry's lifetime.
        let entry = unsafe { mem::transmute::<*const u8, Entry>(memory.address()) };
        Some(Self {
            memory,
            entry,
            instruction_count: compiled.instruction_count,
        })
    }

    pub(crate) const fn mapped_len(&self) -> usize {
        self.memory.len()
    }

    pub(crate) const fn instruction_count(&self) -> usize {
        self.instruction_count
    }

    pub(crate) fn execute(&self, registers: &mut [u32; 32]) -> u32 {
        // SAFETY: The entry follows the private ABI, its RX mapping is alive,
        // and `registers` is exclusively borrowed for the synchronous call.
        unsafe { (self.entry)(registers.as_mut_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::memory::IMAGE_START;

    use super::NativeBlock;
    use crate::{
        block::BasicBlock,
        test_support::{NOP, addi, machine_with_code_at},
    };

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

    fn upper_immediate(opcode: u32, rd: u32, value: u32) -> u32 {
        (value & 0xffff_f000) | (rd << 7) | opcode
    }

    fn immediate(rd: u32, rs1: u32, funct3: u32, immediate: u32) -> u32 {
        ((immediate & 0xfff) << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x13
    }

    fn register(rd: u32, rs1: u32, rs2: u32, funct3: u32, funct7: u32) -> u32 {
        (funct7 << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x33
    }

    fn assert_matches_interpreter(code: &[u32], registers: &[(usize, u32)]) {
        let machine = machine_with_code_at(code, IMAGE_START);
        let block = BasicBlock::translate(&machine, IMAGE_START);
        let native = NativeBlock::compile(&block, usize::MAX).unwrap();
        let mut expected = machine_with_code_at(code, IMAGE_START);
        for &(register, value) in registers {
            expected.registers[register] = value;
        }
        let mut actual_registers = expected.registers;

        for _ in 0..native.instruction_count() {
            let instruction = expected.fetch_decode(expected.pc);
            assert!(expected.execute_one(instruction).is_none());
        }
        let actual_pc = native.execute(&mut actual_registers);

        assert_eq!(actual_registers, expected.registers);
        assert_eq!(actual_pc, expected.pc);
    }

    #[test]
    fn executes_upper_immediates_jumps_and_fence() {
        assert_matches_interpreter(&[upper_immediate(0x37, 5, 0x8123_4000), NOP], &[]);
        assert_matches_interpreter(&[upper_immediate(0x17, 5, 0xffff_f000), NOP], &[]);
        assert_matches_interpreter(&[0x0000_000f, NOP], &[]);
        assert_matches_interpreter(&[NOP, jal(5, 8)], &[]);
    }

    #[test]
    fn executes_immediate_operations() {
        let cases = [
            (addi(5, 6, -1), 0),
            (immediate(5, 6, 2, 0xfff), 0x8000_0000),
            (immediate(5, 6, 3, 0xfff), 0xffff_fffe),
            (immediate(5, 6, 4, 0x55a), 0xaa55_aa55),
            (immediate(5, 6, 6, 0x055), 0xaa00_aa00),
            (immediate(5, 6, 7, 0x0ff), 0xaa55_aa55),
            (immediate(5, 6, 1, 31), 1),
            (immediate(5, 6, 5, 31), 0x8000_0000),
            (immediate(5, 6, 5, (0x20 << 5) | 31), 0x8000_0000),
        ];

        for (instruction, source) in cases {
            assert_matches_interpreter(&[instruction, NOP], &[(6, source)]);
        }
    }

    #[test]
    fn executes_register_operations() {
        let cases = [
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

        for (instruction, left, right) in cases {
            assert_matches_interpreter(&[instruction, NOP], &[(6, left), (7, right)]);
        }
    }

    #[test]
    fn executes_all_branch_conditions() {
        let cases = [
            (0, (5, 5), (5, 6)),
            (1, (5, 6), (5, 5)),
            (4, (u32::MAX, 0), (0, u32::MAX)),
            (5, (0, u32::MAX), (u32::MAX, 0)),
            (6, (0, 1), (1, 0)),
            (7, (1, 0), (0, 1)),
        ];

        for (funct3, taken, not_taken) in cases {
            let code = [NOP, branch(funct3, 6, 7, 8)];
            assert_matches_interpreter(&code, &[(6, taken.0), (7, taken.1)]);
            assert_matches_interpreter(&code, &[(6, not_taken.0), (7, not_taken.1)]);
        }
    }
}
