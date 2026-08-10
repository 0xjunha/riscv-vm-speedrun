use rv32vm_rust_common::{
    GuestTrap,
    machine::{DecodedInstruction, Machine},
    memory::PAGE_SIZE,
};

pub(crate) const MAX_BLOCK_INSTRUCTIONS: usize = 64;

pub(crate) type BlockInstruction = Result<DecodedInstruction, GuestTrap>;

/// A bounded sequence of decoded instructions executed as one unit.
pub(crate) struct BasicBlock {
    instructions: Box<[BlockInstruction]>,
}

impl BasicBlock {
    pub(crate) fn translate(machine: &Machine, start_pc: u32) -> Self {
        let mut instructions = Vec::with_capacity(MAX_BLOCK_INSTRUCTIONS);
        let mut pc = start_pc;

        loop {
            let instruction = machine.fetch_decode(pc);
            let ends_block = instruction
                .as_ref()
                .map_or(true, |instruction| instruction.ends_block());
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
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::memory::{IMAGE_START, PAGE_SIZE};

    use super::{BasicBlock, MAX_BLOCK_INSTRUCTIONS};
    use crate::test_support::{NOP, addi, machine_with_code_at};

    #[test]
    fn translation_stops_at_control_flow() {
        let branch = 0x0000_0063;
        let machine = machine_with_code_at(&[addi(5, 0, 1), branch, NOP], IMAGE_START);

        assert_eq!(
            BasicBlock::translate(&machine, IMAGE_START)
                .instructions()
                .len(),
            2
        );
    }

    #[test]
    fn translation_is_bounded_by_pages_and_length() {
        let page_start = IMAGE_START + PAGE_SIZE as u32 - 8;
        let machine = machine_with_code_at(&[NOP, NOP, NOP], page_start);
        assert_eq!(
            BasicBlock::translate(&machine, page_start)
                .instructions()
                .len(),
            2
        );

        let machine = machine_with_code_at(&vec![NOP; MAX_BLOCK_INSTRUCTIONS + 1], IMAGE_START);
        assert_eq!(
            BasicBlock::translate(&machine, IMAGE_START)
                .instructions()
                .len(),
            MAX_BLOCK_INSTRUCTIONS
        );
    }
}
