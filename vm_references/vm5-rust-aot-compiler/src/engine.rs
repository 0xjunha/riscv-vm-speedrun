//! Executes eager native blocks with precise one-instruction fallback.

use rv32vm_rust_common::{
    machine::{Engine, Machine, RunResult, Termination},
    memory::Image,
};

use crate::aot::NativeImage;

/// Compiles native blocks during `LOAD` and reuses them across runs.
#[derive(Default)]
pub(crate) struct AotCompiler {
    native: NativeImage,
    #[cfg(test)]
    native_retired: u64,
}

impl Engine for AotCompiler {
    fn prepare(&mut self, image: &Image) -> Result<(), String> {
        self.native = NativeImage::prepare(image);
        #[cfg(test)]
        {
            self.native_retired = 0;
        }
        Ok(())
    }

    fn run(&mut self, machine: &mut Machine, instruction_limit: u64) -> RunResult {
        loop {
            if machine.retired >= instruction_limit {
                return machine.result(Termination::InstructionLimit);
            }

            let remaining = instruction_limit - machine.retired;
            if let Some(native) = self.native.get(machine.pc)
                && native.instruction_count() as u64 <= remaining
            {
                let retired = native.instruction_count() as u64;
                machine.pc = native.execute(&mut machine.registers);
                machine.registers[0] = 0;
                machine.retired += retired;
                #[cfg(test)]
                {
                    self.native_retired += retired;
                }
                continue;
            }

            let instruction = machine.fetch_decode(machine.pc);
            if let Some(termination) = machine.execute_one(instruction) {
                return machine.result(termination);
            }
        }
    }
}

#[cfg(all(
    test,
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
impl AotCompiler {
    const fn native_retired(&self) -> u64 {
        self.native_retired
    }
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::{
        machine::{Engine, Machine, Termination},
        memory::IMAGE_START,
    };

    use super::AotCompiler;
    use crate::test_support::{addi, image_with_code_at, lw, machine_with_code_at};

    #[test]
    fn exact_budget_stops_at_the_requested_instruction() {
        let image = image_with_code_at(&[addi(5, 5, 1), addi(5, 5, 1), addi(5, 5, 1)], IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);

        let result = engine.run(&mut machine, 2);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.pc, IMAGE_START + 8);
        assert_eq!(machine.registers[5], 2);
    }

    #[test]
    fn fallback_preserves_trap_retirement() {
        let mut machine = machine_with_code_at(&[addi(5, 0, 7), lw(6, 0, 1)], IMAGE_START);
        let mut engine = AotCompiler::default();

        let result = engine.run(&mut machine, 2);

        assert!(matches!(result.termination, Termination::Trap(_)));
        assert_eq!(result.retired, 1);
        assert_eq!(machine.registers[5], 7);
        assert_eq!(machine.registers[6], 0);
    }

    #[test]
    fn prepare_eagerly_replaces_staged_blocks() {
        let image = image_with_code_at(&[addi(5, 5, 1), addi(5, 5, 1), 0x0000_0073], IMAGE_START);
        let mut engine = AotCompiler::default();

        engine.prepare(&image).unwrap();

        assert_eq!(engine.native.staged_block_count(), 1);

        let fallback = image_with_code_at(&[0x0000_0073], IMAGE_START);
        engine.prepare(&fallback).unwrap();
        assert_eq!(engine.native.staged_block_count(), 0);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn dispatches_native_blocks_with_precise_pc_and_retirement() {
        use crate::test_support::beq;

        let image = image_with_code_at(&[addi(5, 5, 1), beq(0, 0, -4)], IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);

        let result = engine.run(&mut machine, 2);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.pc, IMAGE_START);
        assert_eq!(machine.registers[5], 1);
        assert_eq!(machine.retired, 2);
        assert_eq!(engine.native_retired(), 2);
    }
}
