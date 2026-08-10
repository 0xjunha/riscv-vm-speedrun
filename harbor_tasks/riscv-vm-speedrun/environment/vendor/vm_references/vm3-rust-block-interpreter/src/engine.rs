use rv32vm_rust_common::{
    machine::{Engine, Machine, RunResult, Termination},
    memory::{ADDRESS_SPACE_SIZE, Image},
};

use crate::{block::BasicBlock, cache::BlockCache};

/// Executes guest programs through cached decoded instruction blocks.
#[derive(Default)]
pub(crate) struct BlockInterpreter {
    cache: BlockCache,
}

impl BlockInterpreter {
    fn execute_block(
        machine: &mut Machine,
        instruction_limit: u64,
        block: &BasicBlock,
    ) -> Option<RunResult> {
        let remaining = instruction_limit - machine.retired;
        let permitted = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(block.instructions().len());

        for &instruction in &block.instructions()[..permitted] {
            if let Some(termination) = machine.execute_one(instruction) {
                return Some(machine.result(termination));
            }
        }
        (permitted < block.instructions().len())
            .then(|| machine.result(Termination::InstructionLimit))
    }
}

impl Engine for BlockInterpreter {
    fn prepare(&mut self, _image: &Image) -> Result<(), String> {
        self.cache.clear();
        Ok(())
    }

    fn run(&mut self, machine: &mut Machine, instruction_limit: u64) -> RunResult {
        loop {
            if machine.retired >= instruction_limit {
                return machine.result(Termination::InstructionLimit);
            }

            let pc = machine.pc;
            if pc & 3 != 0 || pc >= ADDRESS_SPACE_SIZE {
                let instruction = machine.fetch_decode(pc);
                if let Some(termination) = machine.execute_one(instruction) {
                    return machine.result(termination);
                }
                continue;
            }

            let block = self.cache.get_or_translate(machine, pc);
            if let Some(result) = Self::execute_block(machine, instruction_limit, block.block()) {
                return result;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::machine::{Engine, Termination};
    use rv32vm_rust_common::memory::IMAGE_START;

    use super::BlockInterpreter;
    use crate::test_support::{NOP, addi, lw, machine_with_code_at};

    #[test]
    fn exact_budget_stops_at_a_block_prefix() {
        let mut machine = machine_with_code_at(
            &[addi(5, 5, 1), addi(5, 5, 1), addi(5, 5, 1), 0x0000_0073],
            IMAGE_START,
        );
        let mut engine = BlockInterpreter::default();

        let result = engine.run(&mut machine, 2);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.pc, IMAGE_START + 8);
        assert_eq!(machine.registers[5], 2);
        assert_eq!(engine.cache.translation_count(), 1);
    }

    #[test]
    fn cache_is_reused_across_runs() {
        let code = [addi(5, 0, 1), 0x0000_0073];
        let mut engine = BlockInterpreter::default();

        let mut first = machine_with_code_at(&code, IMAGE_START);
        engine.run(&mut first, 2);
        let mut second = machine_with_code_at(&code, IMAGE_START);
        engine.run(&mut second, 2);

        assert_eq!(engine.cache.translation_count(), 1);
    }

    #[test]
    fn a_trap_does_not_retire_the_faulting_instruction() {
        let mut machine = machine_with_code_at(&[addi(5, 0, 7), lw(6, 0, 1), NOP], IMAGE_START);
        let mut engine = BlockInterpreter::default();

        let result = engine.run(&mut machine, 3);

        assert!(matches!(result.termination, Termination::Trap(_)));
        assert_eq!(result.retired, 1);
        assert_eq!(machine.registers[5], 7);
        assert_eq!(machine.registers[6], 0);
    }
}
