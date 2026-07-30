//! Owns native-code publication and invocation for x86-64 Linux.

use std::mem;

use crate::{CompiledBlock, memory::ExecutableMemory};

type Entry = unsafe extern "C" fn(*mut u32) -> u32;

#[derive(Clone, Copy)]
struct EntryMetadata {
    offset: usize,
    instruction_count: usize,
}

/// Owns one executable mapping containing one or more native blocks.
pub struct NativeProgram {
    memory: ExecutableMemory,
    entries: Vec<EntryMetadata>,
}

impl NativeProgram {
    pub fn publish(blocks: Vec<CompiledBlock>, code_budget: usize) -> Option<Self> {
        let code_len = blocks.iter().try_fold(0_usize, |length, block| {
            length.checked_add(block.code_len())
        })?;
        if code_len == 0 {
            return None;
        }

        let mut code = Vec::with_capacity(code_len);
        let mut entries = Vec::with_capacity(blocks.len());
        for block in blocks {
            entries.push(EntryMetadata {
                offset: code.len(),
                instruction_count: block.instruction_count(),
            });
            code.extend_from_slice(&block.code);
        }

        let memory = ExecutableMemory::publish(&code, code_budget)?;
        Some(Self { memory, entries })
    }

    pub fn entry(&self, index: usize) -> Option<NativeEntry<'_>> {
        Some(NativeEntry {
            program: self,
            metadata: *self.entries.get(index)?,
        })
    }

    pub const fn mapped_len(&self) -> usize {
        self.memory.len()
    }
}

/// A native block entry tied to the executable program that owns it.
#[derive(Clone, Copy)]
pub struct NativeEntry<'a> {
    program: &'a NativeProgram,
    metadata: EntryMetadata,
}

impl NativeEntry<'_> {
    pub const fn instruction_count(&self) -> usize {
        self.metadata.instruction_count
    }

    /// Executes the native block and returns its next guest program counter.
    pub fn execute(&self, registers: &mut [u32; 32]) -> u32 {
        debug_assert!(self.metadata.offset < self.program.memory.len());
        // SAFETY: Every offset was recorded at the start of a finalized block
        // while assembling this still-live executable program.
        let address = unsafe { self.program.memory.address().add(self.metadata.offset) };
        debug_assert_eq!(size_of::<Entry>(), size_of::<*const u8>());
        // SAFETY: `address` names finalized bytes emitted for `Entry`.
        let entry = unsafe { mem::transmute::<*const u8, Entry>(address) };
        // SAFETY: The entry follows the private ABI, its RX mapping is alive,
        // and `registers` is exclusively borrowed for the synchronous call.
        unsafe { entry(registers.as_mut_ptr()) }
    }
}

/// Owns one executable native block and its guest-instruction count.
pub struct NativeBlock {
    program: NativeProgram,
}

impl NativeBlock {
    pub fn publish(block: CompiledBlock, code_budget: usize) -> Option<Self> {
        let program = NativeProgram::publish(vec![block], code_budget)?;
        Some(Self { program })
    }

    pub const fn mapped_len(&self) -> usize {
        self.program.mapped_len()
    }

    pub fn instruction_count(&self) -> usize {
        self.program
            .entry(0)
            .expect("single-block program has one entry")
            .instruction_count()
    }

    pub fn execute(&self, registers: &mut [u32; 32]) -> u32 {
        self.program
            .entry(0)
            .expect("single-block program has one entry")
            .execute(registers)
    }
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::memory::IMAGE_START;

    use super::{NativeBlock, NativeProgram};
    use crate::CompiledBlock;
    use crate::test_support::{NOP, addi, decoded_block, machine_with_code};

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
        let machine = machine_with_code(code, IMAGE_START);
        let block = decoded_block(&machine, IMAGE_START);
        let compiled = CompiledBlock::compile(&block).unwrap();
        let native = NativeBlock::publish(compiled, usize::MAX).unwrap();
        let mut expected = machine_with_code(code, IMAGE_START);
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

    #[test]
    fn publishes_multiple_blocks_in_one_program() {
        let first_machine = machine_with_code(&[addi(5, 5, 1), NOP], IMAGE_START);
        let second_machine = machine_with_code(&[addi(6, 6, 2), NOP], IMAGE_START);
        let first = CompiledBlock::compile(&decoded_block(&first_machine, IMAGE_START)).unwrap();
        let second = CompiledBlock::compile(&decoded_block(&second_machine, IMAGE_START)).unwrap();

        let program = NativeProgram::publish(vec![first, second], usize::MAX).unwrap();
        let mut registers = [0; 32];

        assert_eq!(
            program.entry(0).unwrap().execute(&mut registers),
            IMAGE_START + 8
        );
        assert_eq!(
            program.entry(1).unwrap().execute(&mut registers),
            IMAGE_START + 8
        );
        assert_eq!(registers[5], 1);
        assert_eq!(registers[6], 2);
        assert!(program.entry(2).is_none());
    }
}
