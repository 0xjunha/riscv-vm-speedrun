//! Dispatches between cached native code and precise interpreted execution.

use rv32vm_rust_common::{
    machine::{Engine, Machine, RunResult, Termination},
    memory::{ADDRESS_SPACE_SIZE, Image},
};

use crate::{
    block::BasicBlock,
    cache::{BlockCache, BlockLookup, CachedBlock},
};

const MAX_CODE_BYTES: usize = 16 * 1024 * 1024;

/// Profiles cached blocks and executes their native tier when available.
#[derive(Default)]
pub(crate) struct JitInterpreter {
    cache: BlockCache,
    code_bytes: usize,
}

impl JitInterpreter {
    fn execute_block(
        machine: &mut Machine,
        instruction_limit: u64,
        block: &BasicBlock,
    ) -> Option<RunResult> {
        let remaining = instruction_limit - machine.retired;
        let permitted = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(block.len());

        for &instruction in &block.instructions()[..permitted] {
            if let Some(termination) = machine.execute_one(instruction) {
                return Some(machine.result(termination));
            }
        }
        (permitted < block.len()).then(|| machine.result(Termination::InstructionLimit))
    }

    fn execute_cached(
        cached: &mut CachedBlock,
        machine: &mut Machine,
        instruction_limit: u64,
        code_bytes: &mut usize,
    ) -> Option<RunResult> {
        let remaining = instruction_limit - machine.retired;
        if remaining < cached.block().len() as u64 {
            return Self::execute_block(machine, instruction_limit, cached.block());
        }

        if let Some(native) = cached.native() {
            let count = native.instruction_count();
            debug_assert!(count <= cached.block().len());
            machine.pc = native.execute(&mut machine.registers);
            machine.registers[0] = 0;
            machine.retired += count as u64;
            None
        } else {
            let result = Self::execute_block(machine, instruction_limit, cached.block());
            if result.is_none() {
                let available = MAX_CODE_BYTES.saturating_sub(*code_bytes);
                *code_bytes += cached.observe_and_compile(available);
            }
            result
        }
    }
}

impl Engine for JitInterpreter {
    fn prepare(&mut self, _image: &Image) -> Result<(), String> {
        self.cache.clear();
        self.code_bytes = 0;
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

            match self.cache.get_or_translate(machine, pc) {
                BlockLookup::Cached(cached) => {
                    if let Some(result) = Self::execute_cached(
                        cached,
                        machine,
                        instruction_limit,
                        &mut self.code_bytes,
                    ) {
                        return result;
                    }
                }
                BlockLookup::Transient(block) => {
                    if let Some(result) = Self::execute_block(machine, instruction_limit, &block) {
                        return result;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::{
        machine::{Engine, Machine, Termination},
        memory::IMAGE_START,
    };

    use super::JitInterpreter;
    use crate::test_support::{NOP, addi, image_with_code_at, lw, machine_with_code_at};

    #[test]
    fn exact_budget_stops_before_the_next_instruction() {
        let mut machine = machine_with_code_at(&[addi(5, 5, 1), addi(5, 5, 1), NOP], IMAGE_START);
        let mut engine = JitInterpreter::default();

        let result = engine.run(&mut machine, 2);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.pc, IMAGE_START + 8);
        assert_eq!(machine.registers[5], 2);
    }

    #[test]
    fn fallback_preserves_trap_retirement() {
        let mut machine = machine_with_code_at(&[addi(5, 0, 7), lw(6, 0, 1), NOP], IMAGE_START);
        let mut engine = JitInterpreter::default();

        let result = engine.run(&mut machine, 3);

        assert!(matches!(result.termination, Termination::Trap(_)));
        assert_eq!(result.retired, 1);
        assert_eq!(machine.registers[5], 7);
        assert_eq!(machine.registers[6], 0);
    }

    #[test]
    fn prepare_clears_decoded_blocks() {
        let image = image_with_code_at(&[addi(5, 0, 1), lw(6, 0, 0)], IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        let mut engine = JitInterpreter::default();

        engine.run(&mut machine, 1);
        assert_eq!(engine.cache.block_count(), 1);

        engine.prepare(&image).unwrap();

        assert_eq!(engine.cache.block_count(), 0);
        assert_eq!(engine.code_bytes, 0);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn promotes_hot_blocks_and_preserves_exact_budgets() {
        use crate::test_support::beq;

        let image = image_with_code_at(&[addi(5, 5, 1), beq(0, 0, -4)], IMAGE_START);
        let mut engine = JitInterpreter::default();

        for _ in 0..3 {
            let mut machine = Machine::new(&image, &[], 0);
            let result = engine.run(&mut machine, 2);
            assert_eq!(result.termination, Termination::InstructionLimit);
            assert_eq!(machine.registers[5], 1);
            assert_eq!(machine.pc, IMAGE_START);
        }
        assert_eq!(engine.cache.native_block_count(), 1);
        assert!(engine.code_bytes > 0);

        let mut short = Machine::new(&image, &[], 0);
        engine.run(&mut short, 1);
        assert_eq!(short.retired, 1);
        assert_eq!(short.registers[5], 1);
        assert_eq!(short.pc, IMAGE_START + 4);

        let mut native = Machine::new(&image, &[], 0);
        engine.run(&mut native, 2);
        assert_eq!(native.retired, 2);
        assert_eq!(native.registers[5], 1);
        assert_eq!(native.pc, IMAGE_START);

        engine.prepare(&image).unwrap();
        assert_eq!(engine.cache.block_count(), 0);
        assert_eq!(engine.cache.native_block_count(), 0);
        assert_eq!(engine.code_bytes, 0);
    }
}
