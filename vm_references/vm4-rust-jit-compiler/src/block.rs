//! Forms immutable decoded blocks for profiling and native compilation.

use rv32vm_rust_common::{
    GuestTrap,
    machine::{DecodedInstruction, Machine},
    memory::PAGE_SIZE,
};

pub(crate) const MAX_BLOCK_INSTRUCTIONS: usize = 64;
pub(crate) type BlockInstruction = Result<DecodedInstruction, GuestTrap>;

/// A bounded sequence of decoded guest instructions.
pub(crate) struct BasicBlock {
    instructions: Box<[BlockInstruction]>,
}

impl BasicBlock {
    pub(crate) fn translate(machine: &Machine, start_pc: u32) -> Self {
        let mut instructions = Vec::with_capacity(MAX_BLOCK_INSTRUCTIONS);
        let mut pc = start_pc;

        loop {
            let instruction = machine.fetch_decode(pc);
            let ends_block = instruction.as_ref().map_or(true, |instruction| {
                instruction.ends_block() || needs_precise_execution(*instruction)
            });
            instructions.push(instruction);
            if ends_block || instructions.len() == MAX_BLOCK_INSTRUCTIONS {
                break;
            }

            pc = pc.wrapping_add(4);
            if pc.is_multiple_of(PAGE_SIZE as u32) {
                break;
            }
        }

        Self {
            instructions: instructions.into_boxed_slice(),
        }
    }

    pub(crate) fn instructions(&self) -> &[BlockInstruction] {
        &self.instructions
    }

    pub(crate) fn len(&self) -> usize {
        self.instructions.len()
    }
}

fn needs_precise_execution(instruction: DecodedInstruction) -> bool {
    matches!(instruction.opcode(), 0x03 | 0x23)
        || (instruction.opcode() == 0x33 && instruction.funct7() == 1 && instruction.funct3() != 0)
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::memory::{IMAGE_START, PAGE_SIZE};

    use super::{BasicBlock, MAX_BLOCK_INSTRUCTIONS};
    use crate::test_support::{NOP, addi, lw, machine_with_code_at};

    #[test]
    fn translation_stops_at_control_flow_and_page_boundaries() {
        let branch = 0x0000_0063;
        let machine = machine_with_code_at(&[addi(5, 0, 1), branch, NOP], IMAGE_START);
        assert_eq!(BasicBlock::translate(&machine, IMAGE_START).len(), 2);

        let page_start = IMAGE_START + PAGE_SIZE as u32 - 8;
        let machine = machine_with_code_at(&[NOP, NOP, NOP], page_start);
        assert_eq!(BasicBlock::translate(&machine, page_start).len(), 2);
    }

    #[test]
    fn translation_has_a_fixed_maximum_length() {
        let machine = machine_with_code_at(&vec![NOP; MAX_BLOCK_INSTRUCTIONS + 1], IMAGE_START);

        assert_eq!(
            BasicBlock::translate(&machine, IMAGE_START).len(),
            MAX_BLOCK_INSTRUCTIONS
        );
    }

    #[test]
    fn translation_isolates_instructions_that_need_precise_execution() {
        let machine = machine_with_code_at(&[addi(5, 0, 1), lw(6, 0, 0), NOP], IMAGE_START);

        assert_eq!(BasicBlock::translate(&machine, IMAGE_START).len(), 2);
        assert_eq!(BasicBlock::translate(&machine, IMAGE_START + 4).len(), 1);
    }
}
