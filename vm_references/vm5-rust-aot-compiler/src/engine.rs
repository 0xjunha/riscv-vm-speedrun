//! Executes eager native blocks with precise one-instruction fallback.

use rv32vm_rust_common::{
    machine::{Engine, Machine, RunResult, Termination},
    memory::Image,
};

use crate::aot::NativeImage;
#[cfg(feature = "profile")]
use crate::profile::RunProfile;

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
        #[cfg(feature = "profile")]
        {
            let (result, profile) = self.run_profiled(machine, instruction_limit);
            profile.emit(
                result.termination,
                result.retired,
                self.native.load_profile(),
            );
            result
        }
        #[cfg(not(feature = "profile"))]
        {
            self.run_unprofiled(machine, instruction_limit)
        }
    }
}

impl AotCompiler {
    #[cfg(not(feature = "profile"))]
    fn run_unprofiled(&mut self, machine: &mut Machine, instruction_limit: u64) -> RunResult {
        self.run_inner(machine, instruction_limit)
    }

    #[cfg(feature = "profile")]
    fn run_profiled(
        &mut self,
        machine: &mut Machine,
        instruction_limit: u64,
    ) -> (RunResult, RunProfile) {
        let mut profile = RunProfile::default();
        let result = self.run_inner(machine, instruction_limit, &mut profile);
        (result, profile)
    }

    /// Executes VM5 with one authoritative set of PC, retirement, budget, and
    /// fallback transitions. Profile-only arguments and events are removed at
    /// compile time from the default steady-state path.
    fn run_inner(
        &mut self,
        machine: &mut Machine,
        instruction_limit: u64,
        #[cfg(feature = "profile")] profile: &mut RunProfile,
    ) -> RunResult {
        let termination = loop {
            if machine.retired >= instruction_limit {
                break Termination::InstructionLimit;
            }

            let remaining = instruction_limit - machine.retired;
            let native = self.native.get(machine.pc);
            #[cfg(feature = "profile")]
            if native.is_none() {
                profile.lookup_fallbacks += 1;
            }
            if let Some(native) = native {
                let native_run = native.execute(&mut machine.registers, machine.pc, remaining);
                debug_assert!(native_run.retired <= remaining);
                machine.pc = native_run.pc;
                machine.registers[0] = 0;
                machine.retired += native_run.retired;
                #[cfg(feature = "profile")]
                profile.record_native_run(native_run.retired, native_run.profile, native_run.stop);
                #[cfg(test)]
                {
                    self.native_retired += native_run.retired;
                }
                if machine.retired >= instruction_limit {
                    continue;
                }
                #[cfg(feature = "profile")]
                match native_run.stop {
                    crate::linked::NativeStop::Budget => profile.budget_fallbacks += 1,
                    crate::linked::NativeStop::MissingSuccessor => {
                        profile.lookup_fallbacks += 1;
                    }
                }
            }

            let instruction = machine.fetch_decode(machine.pc);
            #[cfg(feature = "profile")]
            profile.record_fallback(&instruction);
            #[cfg(feature = "profile")]
            let retired_before = machine.retired;
            let termination = machine.execute_one(instruction);
            #[cfg(feature = "profile")]
            {
                profile.fallback_retired += machine.retired - retired_before;
            }
            if let Some(termination) = termination {
                break termination;
            }
        };

        machine.result(termination)
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

    #[cfg(feature = "profile")]
    #[test]
    fn profile_accounts_for_each_fallback_termination_path() {
        let image = image_with_code_at(&[0x0000_0073], IMAGE_START);
        let mut exit_machine = Machine::new(&image, &[], 0);
        let mut engine = AotCompiler::default();

        let (exit, exit_profile) = engine.run_profiled(&mut exit_machine, 1);

        assert!(matches!(exit.termination, Termination::Exit(_)));
        assert_eq!(exit_profile.lookup_fallbacks, 1);
        assert_eq!(exit_profile.fallback_retired, 1);
        assert_eq!(exit_profile.fallback_system, 1);

        let trap_image = image_with_code_at(&[lw(5, 0, 1)], IMAGE_START);
        let mut trap_machine = Machine::new(&trap_image, &[], 0);
        let (trap, trap_profile) = engine.run_profiled(&mut trap_machine, 1);

        assert!(matches!(trap.termination, Termination::Trap(_)));
        assert_eq!(trap_profile.lookup_fallbacks, 1);
        assert_eq!(trap_profile.fallback_retired, 0);
        assert_eq!(trap_profile.fallback_loads, 1);

        let mut limited_machine = Machine::new(&image, &[], 0);
        let (limited, limited_profile) = engine.run_profiled(&mut limited_machine, 0);

        assert_eq!(limited.termination, Termination::InstructionLimit);
        assert_eq!(limited.retired, 0);
        assert_eq!(limited_profile.native_dispatches, 0);
        assert_eq!(limited_profile.lookup_fallbacks, 0);
        assert_eq!(limited_profile.budget_fallbacks, 0);
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

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn reenters_native_after_exactly_one_fallback_instruction() {
        let image = image_with_code_at(
            &[
                addi(5, 5, 1),
                addi(5, 5, 1),
                lw(6, 10, 0),
                addi(7, 7, 1),
                addi(7, 7, 1),
                0x0000_0073,
            ],
            IMAGE_START,
        );
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[10] = IMAGE_START;

        let result = engine.run(&mut machine, 5);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.pc, IMAGE_START + 20);
        assert_eq!(machine.retired, 5);
        assert_eq!(machine.registers[5], 2);
        assert_eq!(machine.registers[6], addi(5, 5, 1));
        assert_eq!(machine.registers[7], 2);
        assert_eq!(engine.native_retired(), 4);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn committed_missing_target_observes_instruction_limit_before_fetch_trap() {
        use rv32vm_rust_common::GuestTrap;

        use crate::test_support::jal;

        let target = IMAGE_START + 0x1_000;
        let jump_pc = IMAGE_START + 4;
        let image = image_with_code_at(
            &[addi(6, 6, 1), jal(5, (target - jump_pc) as i32)],
            IMAGE_START,
        );
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();

        let mut limited = Machine::new(&image, &[], 0);
        let limited_result = engine.run(&mut limited, 2);
        assert_eq!(limited_result.termination, Termination::InstructionLimit);
        assert_eq!(limited.pc, target);
        assert_eq!(limited.registers[5], IMAGE_START + 8);
        assert_eq!(limited.retired, 2);

        let mut trapping = Machine::new(&image, &[], 0);
        let trapping_result = engine.run(&mut trapping, 3);
        assert_eq!(
            trapping_result.termination,
            Termination::Trap(GuestTrap::new("InstructionAccessFault", target, target))
        );
        assert_eq!(trapping.pc, target);
        assert_eq!(trapping.registers[5], IMAGE_START + 8);
        assert_eq!(trapping.retired, 2);
    }

    #[cfg(all(
        feature = "profile",
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn profile_weights_native_block_traffic_and_separates_budget_fallback() {
        use crate::test_support::beq;

        let image = image_with_code_at(&[addi(5, 5, 1), beq(0, 0, -4)], IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();

        let mut budget_machine = Machine::new(&image, &[], 0);
        let (budget_result, budget_profile) = engine.run_profiled(&mut budget_machine, 1);
        assert_eq!(budget_result.termination, Termination::InstructionLimit);
        assert_eq!(budget_profile.budget_fallbacks, 1);
        assert_eq!(budget_profile.lookup_fallbacks, 0);
        assert_eq!(budget_profile.fallback_retired, 1);

        let mut native_machine = Machine::new(&image, &[], 0);
        let (native_result, native_profile) = engine.run_profiled(&mut native_machine, 2);
        assert_eq!(native_result.termination, Termination::InstructionLimit);
        assert_eq!(native_profile.native_retired, 2);
        assert_eq!(native_profile.native_invocations, 1);
        assert_eq!(native_profile.native_dispatches, 1);
        assert_eq!(native_profile.native_direct_link_hits, 1);
        assert_eq!(native_profile.native_budget_exits, 1);
        assert_eq!(native_profile.native_branch_dispatches, 1);
        assert_eq!(native_profile.generated_guest_register_loads, 3);
        assert_eq!(native_profile.generated_guest_register_stores, 1);
    }

    #[cfg(all(
        feature = "profile",
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn profile_counts_multiple_blocks_in_one_native_invocation() {
        use crate::test_support::beq;

        let image = image_with_code_at(
            &[
                addi(5, 5, 1),
                beq(0, 0, 8),
                lw(6, 0, 0),
                addi(7, 7, 1),
                addi(7, 7, 1),
                0x0000_0073,
            ],
            IMAGE_START,
        );
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);

        let (result, profile) = engine.run_profiled(&mut machine, 4);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.pc, IMAGE_START + 20);
        assert_eq!(profile.native_retired, 4);
        assert_eq!(profile.native_invocations, 1);
        assert_eq!(profile.native_dispatches, 2);
        assert_eq!(profile.native_direct_link_hits, 1);
        assert_eq!(profile.native_missing_exits, 1);
        assert_eq!(profile.native_branch_dispatches, 1);
        assert_eq!(profile.native_fallthrough_dispatches, 1);
        assert_eq!(profile.generated_guest_register_loads, 5);
        assert_eq!(profile.generated_guest_register_stores, 3);
    }
}
