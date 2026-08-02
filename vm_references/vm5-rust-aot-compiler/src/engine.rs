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
                let native_run = native.execute(
                    &mut machine.registers,
                    &mut machine.memory,
                    machine.pc,
                    remaining,
                );
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
                    crate::linked::NativeStop::InterpretOne => {}
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
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use rv32vm_rust_common::memory::{
        ADDRESS_SPACE_SIZE, INPUT_START, Image, PAGE_SHIFT, PAGE_SIZE, PERM_READ, PERM_WRITE,
        STACK_START,
    };
    use rv32vm_rust_common::{
        machine::{Engine, Machine, Termination},
        memory::IMAGE_START,
    };

    use super::AotCompiler;
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use crate::test_support::jalr;
    use crate::test_support::{addi, image_with_code_at, lw, machine_with_code_at};

    #[cfg(any(
        feature = "profile",
        all(
            target_arch = "x86_64",
            target_os = "linux",
            target_pointer_width = "64"
        )
    ))]
    fn load(rd: u32, rs1: u32, funct3: u32, immediate: i32) -> u32 {
        ((immediate as u32 & 0xfff) << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x03
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    fn store(rs1: u32, rs2: u32, funct3: u32, immediate: i32) -> u32 {
        let immediate = immediate as u32 & 0xfff;
        ((immediate >> 5) << 25)
            | (rs2 << 20)
            | (rs1 << 15)
            | (funct3 << 12)
            | ((immediate & 0x1f) << 7)
            | 0x23
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    fn singleton_control_flow_image() -> Image {
        use crate::test_support::{beq, jal};

        image_with_code_at(
            &[
                lw(5, 10, 0),
                beq(5, 0, 8),
                0x0000_0073,
                jal(6, 8),
                0x0000_0073,
                addi(7, 7, 1),
                0x0000_0073,
            ],
            IMAGE_START,
        )
    }

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

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn singleton_arithmetic_observes_exact_zero_and_one_budgets() {
        let image = image_with_code_at(&[addi(5, 5, 1), 0x0000_0073], IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();

        let mut zero = Machine::new(&image, &[], 0);
        let zero_result = engine.run(&mut zero, 0);
        assert_eq!(zero_result.termination, Termination::InstructionLimit);
        assert_eq!(zero.pc, IMAGE_START);
        assert_eq!(zero.registers[5], 0);
        assert_eq!(zero.retired, 0);
        assert_eq!(engine.native_retired(), 0);

        let mut one = Machine::new(&image, &[], 0);
        let one_result = engine.run(&mut one, 1);
        assert_eq!(one_result.termination, Termination::InstructionLimit);
        assert_eq!(one.pc, IMAGE_START + 4);
        assert_eq!(one.registers[5], 1);
        assert_eq!(one.retired, 1);
        assert_eq!(engine.native_retired(), 1);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn singleton_branch_and_jal_link_after_checked_memory() {
        let image = singleton_control_flow_image();
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);

        let result = engine.run(&mut machine, 4);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.pc, IMAGE_START + 24);
        assert_eq!(machine.registers[5], 0);
        assert_eq!(machine.registers[6], IMAGE_START + 16);
        assert_eq!(machine.registers[7], 1);
        assert_eq!(machine.retired, 4);
        assert_eq!(engine.native_retired(), 4);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn short_successor_budget_falls_back_at_the_exact_linked_pc() {
        use crate::test_support::jal;

        let image = image_with_code_at(
            &[
                jal(0, 4),
                addi(6, 6, 1),
                addi(6, 6, 1),
                addi(6, 6, 1),
                0x0000_0073,
            ],
            IMAGE_START,
        );
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);

        let result = engine.run(&mut machine, 2);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.pc, IMAGE_START + 8);
        assert_eq!(machine.retired, 2);
        assert_eq!(machine.registers[6], 1);
        assert_eq!(engine.native_retired(), 1);
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

        let trap_image = image_with_code_at(&[load(5, 0, 3, 0)], IMAGE_START);
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
    fn links_native_jalr_without_interpreter_reentry() {
        let image = image_with_code_at(
            &[
                addi(5, 5, 1),
                addi(5, 5, 1),
                jalr(6, 10, 0),
                addi(7, 7, 1),
                addi(7, 7, 1),
                0x0000_0073,
            ],
            IMAGE_START,
        );
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[10] = IMAGE_START + 12;

        let result = engine.run(&mut machine, 5);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.pc, IMAGE_START + 20);
        assert_eq!(machine.retired, 5);
        assert_eq!(machine.registers[5], 2);
        assert_eq!(machine.registers[6], IMAGE_START + 12);
        assert_eq!(machine.registers[7], 2);
        assert_eq!(engine.native_retired(), 5);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn jalr_uses_the_old_aliasing_source_and_clears_bit_zero() {
        let image = image_with_code_at(
            &[jalr(5, 5, 0), 0x0000_0073, addi(6, 6, 1), 0x0000_0073],
            IMAGE_START,
        );
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[5] = IMAGE_START + 9;

        let result = engine.run(&mut machine, 2);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.pc, IMAGE_START + 12);
        assert_eq!(machine.registers[5], IMAGE_START + 4);
        assert_eq!(machine.registers[6], 1);
        assert_eq!(machine.retired, 2);
        assert_eq!(engine.native_retired(), 2);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn misaligned_jalr_traps_before_link_write_or_retirement() {
        use rv32vm_rust_common::GuestTrap;

        let image = image_with_code_at(&[addi(7, 7, 1), jalr(5, 10, 0), 0x0000_0073], IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[5] = 0xfeed_face;
        machine.registers[10] = IMAGE_START + 3;

        let result = engine.run(&mut machine, 3);

        assert_eq!(
            result.termination,
            Termination::Trap(GuestTrap::new(
                "InstructionAddressMisaligned",
                IMAGE_START + 4,
                IMAGE_START + 2,
            ))
        );
        assert_eq!(machine.pc, IMAGE_START + 4);
        assert_eq!(machine.registers[5], 0xfeed_face);
        assert_eq!(machine.registers[7], 1);
        assert_eq!(machine.retired, 1);
        assert_eq!(engine.native_retired(), 1);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn committed_jalr_observes_limit_before_invalid_target_fetch() {
        use rv32vm_rust_common::GuestTrap;

        let image = image_with_code_at(&[jalr(5, 10, 0)], IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();

        let mut limited = Machine::new(&image, &[], 0);
        limited.registers[10] = ADDRESS_SPACE_SIZE;
        let limited_result = engine.run(&mut limited, 1);
        assert_eq!(limited_result.termination, Termination::InstructionLimit);
        assert_eq!(limited.pc, ADDRESS_SPACE_SIZE);
        assert_eq!(limited.registers[5], IMAGE_START + 4);
        assert_eq!(limited.retired, 1);

        let mut trapping = Machine::new(&image, &[], 0);
        trapping.registers[10] = ADDRESS_SPACE_SIZE;
        let trapping_result = engine.run(&mut trapping, 2);
        assert_eq!(
            trapping_result.termination,
            Termination::Trap(GuestTrap::new(
                "InstructionAccessFault",
                ADDRESS_SPACE_SIZE,
                ADDRESS_SPACE_SIZE,
            ))
        );
        assert_eq!(trapping.pc, ADDRESS_SPACE_SIZE);
        assert_eq!(trapping.registers[5], IMAGE_START + 4);
        assert_eq!(trapping.retired, 1);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn uncompiled_interior_jalr_target_exits_after_committing() {
        let image = image_with_code_at(
            &[jalr(5, 10, 0), addi(6, 6, 1), addi(7, 7, 1), 0x0000_0073],
            IMAGE_START,
        );
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[10] = IMAGE_START + 8;

        let result = engine.run(&mut machine, 2);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.pc, IMAGE_START + 12);
        assert_eq!(machine.registers[5], IMAGE_START + 4);
        assert_eq!(machine.registers[6], 0);
        assert_eq!(machine.registers[7], 1);
        assert_eq!(machine.retired, 2);
        assert_eq!(engine.native_retired(), 1);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn jalr_differential_matrix_matches_the_interpreter() {
        struct Case {
            name: &'static str,
            instruction: u32,
            source: u32,
            limit: u64,
        }

        fn run_interpreter(machine: &mut Machine, instruction_limit: u64) -> Termination {
            loop {
                if machine.retired >= instruction_limit {
                    return Termination::InstructionLimit;
                }
                let instruction = machine.fetch_decode(machine.pc);
                if let Some(termination) = machine.execute_one(instruction) {
                    return termination;
                }
            }
        }

        let target = IMAGE_START + 4;
        let cases = [
            Case {
                name: "rd_x0",
                instruction: jalr(0, 10, 0),
                source: target,
                limit: 2,
            },
            Case {
                name: "minimum_immediate",
                instruction: jalr(5, 10, -2_048),
                source: target.wrapping_add(2_048),
                limit: 2,
            },
            Case {
                name: "maximum_immediate",
                instruction: jalr(5, 10, 2_047),
                source: target.wrapping_sub(2_047),
                limit: 2,
            },
            Case {
                name: "wrapping_target",
                instruction: jalr(5, 10, 1),
                source: u32::MAX,
                limit: 2,
            },
            Case {
                name: "self_hit",
                instruction: jalr(5, 10, 0),
                source: IMAGE_START,
                limit: 3,
            },
            Case {
                name: "zero_budget",
                instruction: jalr(5, 10, 0),
                source: target,
                limit: 0,
            },
            Case {
                name: "one_instruction_budget",
                instruction: jalr(5, 10, 0),
                source: target,
                limit: 1,
            },
            Case {
                name: "short_target_budget",
                instruction: jalr(5, 10, 0),
                source: target,
                limit: 2,
            },
            Case {
                name: "invalid_funct3",
                instruction: jalr(5, 10, 0) | (1 << 12),
                source: target,
                limit: 1,
            },
        ];

        for case in cases {
            let image = image_with_code_at(
                &[case.instruction, addi(6, 6, 1), addi(7, 7, 1), 0x0000_0073],
                IMAGE_START,
            );
            let mut expected = Machine::new(&image, &[], 0);
            let mut actual = Machine::new(&image, &[], 0);
            expected.registers[5] = 0xfeed_face;
            actual.registers[5] = 0xfeed_face;
            expected.registers[10] = case.source;
            actual.registers[10] = case.source;

            let expected_termination = run_interpreter(&mut expected, case.limit);
            let mut engine = AotCompiler::default();
            engine.prepare(&image).unwrap();
            let actual_result = engine.run(&mut actual, case.limit);

            assert_eq!(
                actual_result.termination, expected_termination,
                "termination: {}",
                case.name
            );
            assert_eq!(actual.pc, expected.pc, "pc: {}", case.name);
            assert_eq!(actual.retired, expected.retired, "retired: {}", case.name);
            assert_eq!(
                actual.registers, expected.registers,
                "registers: {}",
                case.name
            );
        }
    }

    #[cfg(all(
        feature = "profile",
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn jalr_profile_distinguishes_hit_miss_and_precise_retry() {
        let hit_image = image_with_code_at(
            &[jalr(5, 10, 0), 0x0000_0073, addi(6, 6, 1), 0x0000_0073],
            IMAGE_START,
        );
        let mut hit_engine = AotCompiler::default();
        hit_engine.prepare(&hit_image).unwrap();
        let mut hit_machine = Machine::new(&hit_image, &[], 0);
        hit_machine.registers[10] = IMAGE_START + 8;

        let (hit_result, hit) = hit_engine.run_profiled(&mut hit_machine, 2);

        assert_eq!(hit_result.termination, Termination::InstructionLimit);
        assert_eq!(hit.native_retired, 2);
        assert_eq!(hit.fallback_retired, 0);
        assert_eq!(hit.native_dispatches, 2);
        assert_eq!(hit.native_indirect_link_hits, 1);
        assert_eq!(hit.native_indirect_link_misses, 0);
        assert_eq!(hit.native_indirect_jump_dispatches, 1);
        assert_eq!(hit.native_missing_exits, 1);
        assert_eq!(hit.generated_guest_register_loads, 2);
        assert_eq!(hit.generated_guest_register_stores, 2);
        assert_eq!(hit.generated_register_cache_fills, 0);
        assert_eq!(hit.generated_register_cache_spills, 0);
        assert_eq!(hit.generated_register_cache_read_hits, 0);
        assert_eq!(hit.generated_register_cache_write_hits, 0);
        assert_eq!(hit.fallback_jalr, 0);

        let miss_image = image_with_code_at(
            &[jalr(5, 10, 0), addi(6, 6, 1), addi(7, 7, 1), 0x0000_0073],
            IMAGE_START,
        );
        let mut miss_engine = AotCompiler::default();
        miss_engine.prepare(&miss_image).unwrap();
        let mut miss_machine = Machine::new(&miss_image, &[], 0);
        miss_machine.registers[10] = IMAGE_START + 8;

        let (miss_result, miss) = miss_engine.run_profiled(&mut miss_machine, 2);

        assert_eq!(miss_result.termination, Termination::InstructionLimit);
        assert_eq!(miss.native_retired, 1);
        assert_eq!(miss.fallback_retired, 1);
        assert_eq!(miss.native_dispatches, 1);
        assert_eq!(miss.native_indirect_link_hits, 0);
        assert_eq!(miss.native_indirect_link_misses, 1);
        assert_eq!(miss.native_indirect_jump_dispatches, 1);
        assert_eq!(miss.native_missing_exits, 1);
        assert_eq!(miss.lookup_fallbacks, 1);
        assert_eq!(miss.generated_guest_register_loads, 1);
        assert_eq!(miss.generated_guest_register_stores, 1);
        assert_eq!(miss.generated_register_cache_fills, 0);
        assert_eq!(miss.generated_register_cache_spills, 0);
        assert_eq!(miss.generated_register_cache_read_hits, 0);
        assert_eq!(miss.generated_register_cache_write_hits, 0);
        assert_eq!(miss.fallback_other, 1);

        let retry_image = image_with_code_at(&[jalr(5, 10, 0)], IMAGE_START);
        let mut retry_engine = AotCompiler::default();
        retry_engine.prepare(&retry_image).unwrap();
        let mut retry_machine = Machine::new(&retry_image, &[], 0);
        retry_machine.registers[5] = 0xfeed_face;
        retry_machine.registers[10] = IMAGE_START + 3;

        let (retry_result, retry) = retry_engine.run_profiled(&mut retry_machine, 1);

        assert!(matches!(retry_result.termination, Termination::Trap(_)));
        assert_eq!(retry.native_retired, 0);
        assert_eq!(retry.fallback_retired, 0);
        assert_eq!(retry.native_dispatches, 1);
        assert_eq!(retry.native_indirect_link_hits, 0);
        assert_eq!(retry.native_indirect_link_misses, 0);
        assert_eq!(retry.native_indirect_jump_dispatches, 0);
        assert_eq!(retry.native_interpret_one_exits, 1);
        assert_eq!(retry.generated_guest_register_loads, 1);
        assert_eq!(retry.generated_guest_register_stores, 0);
        assert_eq!(retry.generated_register_cache_fills, 0);
        assert_eq!(retry.generated_register_cache_spills, 0);
        assert_eq!(retry.generated_register_cache_read_hits, 0);
        assert_eq!(retry.generated_register_cache_write_hits, 0);
        assert_eq!(retry.fallback_jalr, 1);
        assert_eq!(retry_machine.registers[5], 0xfeed_face);
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
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn checked_loads_cover_every_width_and_signedness() {
        let code = [
            load(5, 10, 0, 0),
            load(6, 10, 4, 0),
            load(7, 10, 1, 2),
            load(8, 10, 5, 2),
            load(9, 10, 2, 4),
        ];
        let image = image_with_code_at(&code, IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);
        let base = STACK_START + 0x100;
        machine.registers[10] = base;
        machine.memory.store(base, 1, 0x80, IMAGE_START).unwrap();
        machine
            .memory
            .store(base + 2, 2, 0x8001, IMAGE_START)
            .unwrap();
        machine
            .memory
            .store(base + 4, 4, 0x89ab_cdef, IMAGE_START)
            .unwrap();

        let result = engine.run(&mut machine, code.len() as u64);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.registers[5], 0xffff_ff80);
        assert_eq!(machine.registers[6], 0x80);
        assert_eq!(machine.registers[7], 0xffff_8001);
        assert_eq!(machine.registers[8], 0x8001);
        assert_eq!(machine.registers[9], 0x89ab_cdef);
        assert_eq!(engine.native_retired(), code.len() as u64);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn checked_stores_cover_every_width_and_little_endian_layout() {
        let code = [store(10, 5, 0, 0), store(10, 6, 1, 2), store(10, 7, 2, 4)];
        let image = image_with_code_at(&code, IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);
        let base = STACK_START + 0x180;
        machine.registers[10] = base;
        machine.registers[5] = 0xa5;
        machine.registers[6] = 0xbbaa;
        machine.registers[7] = 0x4433_2211;
        machine.memory.store(base, 1, 0, IMAGE_START).unwrap();

        let result = engine.run(&mut machine, code.len() as u64);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(
            machine.memory.read(base, 8),
            [0xa5, 0, 0xaa, 0xbb, 0x11, 0x22, 0x33, 0x44]
        );
        assert_eq!(engine.native_retired(), code.len() as u64);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn negative_i_and_s_immediates_cover_all_memory_widths() {
        let load_code = [load(5, 10, 0, -1), load(6, 11, 1, -2), load(7, 12, 2, -4)];
        let load_image = image_with_code_at(&load_code, IMAGE_START);
        let mut load_engine = AotCompiler::default();
        load_engine.prepare(&load_image).unwrap();
        let mut load_machine = Machine::new(&load_image, &[], 0);
        let base = STACK_START + 0x800;
        load_machine.registers[10] = base + 1;
        load_machine.registers[11] = base + 4;
        load_machine.registers[12] = base + 8;
        load_machine
            .memory
            .store(base, 1, 0x80, IMAGE_START)
            .unwrap();
        load_machine
            .memory
            .store(base + 2, 2, 0x8001, IMAGE_START)
            .unwrap();
        load_machine
            .memory
            .store(base + 4, 4, 0x89ab_cdef, IMAGE_START)
            .unwrap();

        let result = load_engine.run(&mut load_machine, load_code.len() as u64);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(load_machine.registers[5], 0xffff_ff80);
        assert_eq!(load_machine.registers[6], 0xffff_8001);
        assert_eq!(load_machine.registers[7], 0x89ab_cdef);
        assert_eq!(load_engine.native_retired(), load_code.len() as u64);

        let store_code = [
            store(10, 5, 0, -1),
            store(11, 6, 1, -2),
            store(12, 7, 2, -4),
        ];
        let store_image = image_with_code_at(&store_code, IMAGE_START);
        let mut store_engine = AotCompiler::default();
        store_engine.prepare(&store_image).unwrap();
        let mut store_machine = Machine::new(&store_image, &[], 0);
        let base = STACK_START + 0x900;
        store_machine.registers[10] = base + 1;
        store_machine.registers[11] = base + 4;
        store_machine.registers[12] = base + 8;
        store_machine.registers[5] = 0xa5;
        store_machine.registers[6] = 0xbbaa;
        store_machine.registers[7] = 0x4433_2211;
        store_machine.memory.store(base, 1, 0, IMAGE_START).unwrap();

        let result = store_engine.run(&mut store_machine, store_code.len() as u64);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(
            store_machine.memory.read(base, 8),
            [0xa5, 0, 0xaa, 0xbb, 0x11, 0x22, 0x33, 0x44]
        );
        assert_eq!(store_engine.native_retired(), store_code.len() as u64);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn negative_memory_immediates_preserve_wrapping_and_page_selection() {
        use rv32vm_rust_common::GuestTrap;

        let load_image = image_with_code_at(&[load(5, 10, 2, -4)], IMAGE_START);
        let mut load_engine = AotCompiler::default();
        load_engine.prepare(&load_image).unwrap();
        let mut load_machine = Machine::new(&load_image, &[], 0);
        load_machine.registers[10] = 0;
        load_machine.registers[5] = 0xfeed_face;
        let result = load_engine.run(&mut load_machine, 1);
        assert_eq!(
            result.termination,
            Termination::Trap(GuestTrap::new("LoadAccessFault", IMAGE_START, u32::MAX - 3,))
        );
        assert_eq!(load_machine.registers[5], 0xfeed_face);

        let store_image = image_with_code_at(&[store(10, 5, 2, -4)], IMAGE_START);
        let mut store_engine = AotCompiler::default();
        store_engine.prepare(&store_image).unwrap();
        let mut store_machine = Machine::new(&store_image, &[], 0);
        store_machine.registers[10] = 0;
        store_machine.registers[5] = 0x1122_3344;
        let result = store_engine.run(&mut store_machine, 1);
        assert_eq!(
            result.termination,
            Termination::Trap(GuestTrap::new(
                "StoreAccessFault",
                IMAGE_START,
                u32::MAX - 3,
            ))
        );

        let boundary = IMAGE_START + u32::try_from(PAGE_SIZE * 4).unwrap();
        let mut load_image = image_with_code_at(&[load(5, 10, 0, -1)], IMAGE_START);
        load_image.permissions[(boundary >> PAGE_SHIFT) as usize] = PERM_READ;
        let mut load_engine = AotCompiler::default();
        load_engine.prepare(&load_image).unwrap();
        let mut load_machine = Machine::new(&load_image, &[], 0);
        load_machine.registers[10] = boundary;
        let result = load_engine.run(&mut load_machine, 1);
        assert_eq!(
            result.termination,
            Termination::Trap(GuestTrap::new("LoadAccessFault", IMAGE_START, boundary - 1,))
        );

        let mut store_image = image_with_code_at(&[store(10, 5, 0, -1)], IMAGE_START);
        store_image.permissions[(boundary >> PAGE_SHIFT) as usize] = PERM_WRITE;
        let mut store_engine = AotCompiler::default();
        store_engine.prepare(&store_image).unwrap();
        let mut store_machine = Machine::new(&store_image, &[], 0);
        store_machine.registers[10] = boundary;
        store_machine.registers[5] = 0x55;
        let result = store_engine.run(&mut store_machine, 1);
        assert_eq!(
            result.termination,
            Termination::Trap(GuestTrap::new(
                "StoreAccessFault",
                IMAGE_START,
                boundary - 1,
            ))
        );
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn readable_sparse_loads_return_zero_natively() {
        let image = image_with_code_at(&[load(5, 10, 2, 0)], IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[10] = INPUT_START;
        machine.registers[5] = 0xfeed_face;

        let result = engine.run(&mut machine, 1);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.registers[5], 0);
        assert_eq!(engine.native_retired(), 1);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn sparse_stores_remain_native_across_repeated_runs() {
        let image = image_with_code_at(&[store(10, 5, 2, 0)], IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[10] = STACK_START + 0x200;
        machine.registers[5] = 0x4433_2211;

        #[cfg(not(feature = "profile"))]
        let first = engine.run(&mut machine, 1);
        #[cfg(feature = "profile")]
        let (first, first_profile) = engine.run_profiled(&mut machine, 1);
        assert_eq!(first.termination, Termination::InstructionLimit);
        assert_eq!(first.retired, 1);
        assert_eq!(engine.native_retired(), 1);
        #[cfg(feature = "profile")]
        {
            assert_eq!(first_profile.native_interpret_one_exits, 0);
            assert_eq!(first_profile.native_memory_stores, 1);
        }
        assert_eq!(machine.memory.load_u32(STACK_START + 0x200), 0x4433_2211);

        machine.pc = IMAGE_START;
        machine.retired = 0;
        machine.registers[5] = 0x8877_6655;
        #[cfg(not(feature = "profile"))]
        let second = engine.run(&mut machine, 1);
        #[cfg(feature = "profile")]
        let (second, second_profile) = engine.run_profiled(&mut machine, 1);
        assert_eq!(second.termination, Termination::InstructionLimit);
        assert_eq!(engine.native_retired(), 2);
        #[cfg(feature = "profile")]
        {
            assert_eq!(second_profile.native_interpret_one_exits, 0);
            assert_eq!(second_profile.native_memory_stores, 1);
        }
        assert_eq!(machine.memory.load_u32(STACK_START + 0x200), 0x8877_6655);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn memory_slow_paths_preserve_trap_priority_bounds_and_destination() {
        use rv32vm_rust_common::GuestTrap;

        let cases = [
            (
                load(5, 10, 2, 0),
                1,
                GuestTrap::new("LoadAddressMisaligned", IMAGE_START, 1),
            ),
            (
                load(5, 10, 2, 0),
                ADDRESS_SPACE_SIZE,
                GuestTrap::new("LoadAccessFault", IMAGE_START, ADDRESS_SPACE_SIZE),
            ),
            (
                load(5, 10, 2, 0),
                0,
                GuestTrap::new("LoadAccessFault", IMAGE_START, 0),
            ),
            (
                load(5, 10, 2, 1),
                u32::MAX,
                GuestTrap::new("LoadAccessFault", IMAGE_START, 0),
            ),
        ];
        for (instruction, address, trap) in cases {
            let image = image_with_code_at(&[instruction], IMAGE_START);
            let mut engine = AotCompiler::default();
            engine.prepare(&image).unwrap();
            let mut machine = Machine::new(&image, &[], 0);
            machine.registers[10] = address;
            machine.registers[5] = 0xfeed_face;

            let result = engine.run(&mut machine, 1);

            assert_eq!(result.termination, Termination::Trap(trap));
            assert_eq!(result.retired, 0);
            assert_eq!(machine.registers[5], 0xfeed_face);
            assert_eq!(engine.native_retired(), 0);
        }

        let mut write_only_image = image_with_code_at(&[lw(5, 10, 0)], IMAGE_START);
        let write_only = IMAGE_START + (1 << PAGE_SHIFT);
        write_only_image.permissions[(write_only >> PAGE_SHIFT) as usize] = PERM_WRITE;
        let mut engine = AotCompiler::default();
        engine.prepare(&write_only_image).unwrap();
        let mut machine = Machine::new(&write_only_image, &[], 0);
        machine.registers[10] = write_only;
        machine.registers[5] = 0xfeed_face;
        let result = engine.run(&mut machine, 1);
        assert_eq!(
            result.termination,
            Termination::Trap(GuestTrap::new("LoadAccessFault", IMAGE_START, write_only,))
        );
        assert_eq!(machine.registers[5], 0xfeed_face);

        let store_cases = [
            (1, GuestTrap::new("StoreAddressMisaligned", IMAGE_START, 1)),
            (0, GuestTrap::new("StoreAccessFault", IMAGE_START, 0)),
            (
                IMAGE_START,
                GuestTrap::new("StoreAccessFault", IMAGE_START, IMAGE_START),
            ),
            (
                ADDRESS_SPACE_SIZE,
                GuestTrap::new("StoreAccessFault", IMAGE_START, ADDRESS_SPACE_SIZE),
            ),
        ];
        for (address, trap) in store_cases {
            let image = image_with_code_at(&[store(10, 5, 2, 0)], IMAGE_START);
            let mut engine = AotCompiler::default();
            engine.prepare(&image).unwrap();
            let mut machine = Machine::new(&image, &[], 0);
            machine.registers[10] = address;
            machine.registers[5] = 0x1122_3344;

            let result = engine.run(&mut machine, 1);

            assert_eq!(result.termination, Termination::Trap(trap));
            assert_eq!(result.retired, 0);
            assert_eq!(engine.native_retired(), 0);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn failed_terminal_memory_refunds_only_itself_after_a_native_prefix() {
        use rv32vm_rust_common::GuestTrap;

        let image = image_with_code_at(&[addi(5, 5, 1), lw(6, 10, 0)], IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[10] = 1;
        machine.registers[6] = 0xcafe_babe;

        let result = engine.run(&mut machine, 2);

        assert_eq!(
            result.termination,
            Termination::Trap(GuestTrap::new("LoadAddressMisaligned", IMAGE_START + 4, 1,))
        );
        assert_eq!(result.retired, 1);
        assert_eq!(machine.pc, IMAGE_START + 4);
        assert_eq!(machine.registers[5], 1);
        assert_eq!(machine.registers[6], 0xcafe_babe);
        assert_eq!(engine.native_retired(), 1);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn failed_memory_inside_a_region_refunds_its_uncommitted_suffix() {
        use rv32vm_rust_common::GuestTrap;

        let image = image_with_code_at(
            &[
                addi(5, 5, 1),
                lw(6, 10, 0),
                addi(7, 7, 1),
                lw(8, 11, 0),
                addi(9, 9, 1),
            ],
            IMAGE_START,
        );
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);
        let address = STACK_START + 0x300;
        machine.registers[10] = address;
        machine.registers[11] = 1;
        machine.registers[8] = 0xcafe_babe;
        machine
            .memory
            .store(address, 4, 0x1234_5678, IMAGE_START)
            .unwrap();

        let result = engine.run(&mut machine, 5);

        assert_eq!(
            result.termination,
            Termination::Trap(GuestTrap::new("LoadAddressMisaligned", IMAGE_START + 12, 1,))
        );
        assert_eq!(result.retired, 3);
        assert_eq!(machine.pc, IMAGE_START + 12);
        assert_eq!(machine.registers[5], 1);
        assert_eq!(machine.registers[6], 0x1234_5678);
        assert_eq!(machine.registers[7], 1);
        assert_eq!(machine.registers[8], 0xcafe_babe);
        assert_eq!(machine.registers[9], 0);
        assert_eq!(engine.native_retired(), 3);
    }

    #[cfg(all(
        not(feature = "profile"),
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn region_local_cache_commits_before_a_precise_memory_failure() {
        use rv32vm_rust_common::GuestTrap;

        let mut code = (0..3)
            .flat_map(|_| (1..=7).map(|register| addi(register, register, 1)))
            .collect::<Vec<_>>();
        let load_pc = IMAGE_START + u32::try_from(code.len() * 4).unwrap();
        code.extend_from_slice(&[lw(8, 10, 0), 0x0000_0073]);
        let image = image_with_code_at(&code, IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[8] = 0xcafe_babe;
        machine.registers[10] = 1;

        let result = engine.run(&mut machine, code.len() as u64);

        assert_eq!(
            result.termination,
            Termination::Trap(GuestTrap::new("LoadAddressMisaligned", load_pc, 1))
        );
        assert_eq!(result.retired, 21);
        assert_eq!(machine.pc, load_pc);
        assert_eq!(machine.registers[1], 3);
        assert_eq!(machine.registers[2], ADDRESS_SPACE_SIZE + 3);
        assert_eq!(&machine.registers[3..=7], &[3; 5]);
        assert_eq!(machine.registers[8], 0xcafe_babe);
        assert_eq!(engine.native_retired(), 21);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn short_budget_falls_back_before_prefix_without_over_retirement() {
        let image = image_with_code_at(&[addi(5, 5, 1), lw(6, 10, 0)], IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);
        let address = STACK_START + 0x280;
        machine.registers[10] = address;
        machine
            .memory
            .store(address, 4, 0x1234_5678, IMAGE_START)
            .unwrap();

        let prefix = engine.run(&mut machine, 1);
        assert_eq!(prefix.termination, Termination::InstructionLimit);
        assert_eq!(machine.pc, IMAGE_START + 4);
        assert_eq!(machine.registers[5], 1);
        assert_eq!(machine.registers[6], 0);
        assert_eq!(engine.native_retired(), 0);

        let load = engine.run(&mut machine, 2);
        assert_eq!(load.termination, Termination::InstructionLimit);
        assert_eq!(machine.registers[6], 0x1234_5678);
        assert_eq!(engine.native_retired(), 0);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn repeated_runs_reload_object_specific_memory_anchors() {
        let image = image_with_code_at(&[lw(5, 10, 0)], IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let address = STACK_START + 0x300;

        for expected in [0x1122_3344, 0xaabb_ccdd] {
            let mut machine = Machine::new(&image, &[], 0);
            machine.registers[10] = address;
            machine
                .memory
                .store(address, 4, expected, IMAGE_START)
                .unwrap();

            let result = engine.run(&mut machine, 1);

            assert_eq!(result.termination, Termination::InstructionLimit);
            assert_eq!(machine.registers[5], expected);
        }
        assert_eq!(engine.native_retired(), 2);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn checked_memory_preserves_register_aliases_and_x0() {
        let load_image = image_with_code_at(&[lw(5, 5, 0)], IMAGE_START);
        let mut load_engine = AotCompiler::default();
        load_engine.prepare(&load_image).unwrap();
        let mut load_machine = Machine::new(&load_image, &[], 0);
        let address = STACK_START + 0x380;
        load_machine.registers[5] = address;
        load_machine
            .memory
            .store(address, 4, 0x7654_3210, IMAGE_START)
            .unwrap();
        load_engine.run(&mut load_machine, 1);
        assert_eq!(load_machine.registers[5], 0x7654_3210);

        let x0_image = image_with_code_at(&[lw(0, 10, 0)], IMAGE_START);
        let mut x0_engine = AotCompiler::default();
        x0_engine.prepare(&x0_image).unwrap();
        let mut x0_machine = Machine::new(&x0_image, &[], 0);
        x0_machine.registers[10] = INPUT_START;
        x0_engine.run(&mut x0_machine, 1);
        assert_eq!(x0_machine.registers[0], 0);
        assert_eq!(x0_engine.native_retired(), 1);

        let store_image = image_with_code_at(&[store(5, 5, 2, 0), store(10, 0, 2, 4)], IMAGE_START);
        let mut store_engine = AotCompiler::default();
        store_engine.prepare(&store_image).unwrap();
        let mut store_machine = Machine::new(&store_image, &[], 0);
        let store_address = STACK_START + 0x400;
        store_machine.registers[5] = store_address;
        store_machine.registers[10] = store_address;
        store_machine
            .memory
            .store(store_address, 4, 1, IMAGE_START)
            .unwrap();
        store_engine.run(&mut store_machine, 2);
        assert_eq!(store_machine.memory.load_u32(store_address), store_address);
        assert_eq!(store_machine.memory.load_u32(store_address + 4), 0);
        assert_eq!(store_engine.native_retired(), 2);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn checked_memory_matches_one_step_interpreter_execution() {
        let cases = [
            (load(5, 10, 0, 0), STACK_START + 0x500, 0x0000_0080),
            (load(5, 10, 4, 0), STACK_START + 0x500, 0x0000_0080),
            (load(5, 10, 1, 0), STACK_START + 0x500, 0x0000_8001),
            (load(5, 10, 5, 0), STACK_START + 0x500, 0x0000_8001),
            (load(5, 10, 2, 0), STACK_START + 0x500, 0x89ab_cdef),
            (store(10, 5, 0, 0), STACK_START + 0x500, 0x89ab_cdef),
            (store(10, 5, 1, 0), STACK_START + 0x500, 0x89ab_cdef),
            (store(10, 5, 2, 0), STACK_START + 0x500, 0x89ab_cdef),
            (load(5, 10, 2, 0), 1, 0x89ab_cdef),
            (store(10, 5, 2, 0), 0, 0x89ab_cdef),
        ];

        for (instruction, address, value) in cases {
            let image = image_with_code_at(&[instruction], IMAGE_START);
            let mut expected = Machine::new(&image, &[], 0);
            let mut actual = Machine::new(&image, &[], 0);
            expected.registers[10] = address;
            actual.registers[10] = address;
            expected.registers[5] = value;
            actual.registers[5] = value;
            if (STACK_START..ADDRESS_SPACE_SIZE).contains(&address) {
                expected
                    .memory
                    .store(address, 4, value, IMAGE_START)
                    .unwrap();
                actual.memory.store(address, 4, value, IMAGE_START).unwrap();
            }

            let expected_termination = expected.execute_one(expected.fetch_decode(IMAGE_START));
            let mut engine = AotCompiler::default();
            engine.prepare(&image).unwrap();
            let actual_result = engine.run(&mut actual, 1);

            assert_eq!(
                actual_result.termination,
                expected_termination.unwrap_or(Termination::InstructionLimit)
            );
            assert_eq!(actual.pc, expected.pc);
            assert_eq!(actual.registers, expected.registers);
            assert_eq!(actual.retired, expected.retired);
            if (STACK_START..ADDRESS_SPACE_SIZE).contains(&address) {
                assert_eq!(
                    actual.memory.read(address, 4),
                    expected.memory.read(address, 4)
                );
            }
        }
    }

    #[cfg(all(
        feature = "profile",
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn profile_counts_checked_memory_at_exact_dynamic_commit_points() {
        let load_image = image_with_code_at(&[addi(5, 5, 1), lw(6, 10, 0)], IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&load_image).unwrap();
        let mut success = Machine::new(&load_image, &[], 0);
        let address = STACK_START + 0x600;
        success.registers[10] = address;
        success
            .memory
            .store(address, 4, 0x1234_5678, IMAGE_START)
            .unwrap();

        let (result, profile) = engine.run_profiled(&mut success, 2);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(profile.native_retired, 2);
        assert_eq!(profile.generated_guest_register_loads, 2);
        assert_eq!(profile.generated_guest_register_stores, 2);
        assert_eq!(profile.generated_register_cache_read_hits, 0);
        assert_eq!(profile.generated_register_cache_write_hits, 0);
        assert_eq!(profile.native_memory_loads, 1);
        assert_eq!(profile.native_memory_stores, 0);
        assert_eq!(profile.native_fallthrough_dispatches, 1);
        assert_eq!(profile.native_interpret_one_exits, 0);
        assert_eq!(profile.fallback_loads, 0);

        let mut fault = Machine::new(&load_image, &[], 0);
        fault.registers[10] = 1;
        fault.registers[6] = 0xfeed_face;
        let (_, profile) = engine.run_profiled(&mut fault, 2);
        assert_eq!(profile.native_retired, 1);
        assert_eq!(profile.generated_guest_register_loads, 2);
        assert_eq!(profile.generated_guest_register_stores, 1);
        assert_eq!(profile.generated_register_cache_read_hits, 0);
        assert_eq!(profile.generated_register_cache_write_hits, 0);
        assert_eq!(profile.native_memory_loads, 0);
        assert_eq!(profile.native_fallthrough_dispatches, 0);
        assert_eq!(profile.native_interpret_one_exits, 1);
        assert_eq!(profile.fallback_loads, 1);
        assert_eq!(profile.fallback_retired, 0);

        let store_image = image_with_code_at(&[store(10, 5, 2, 0)], IMAGE_START);
        engine.prepare(&store_image).unwrap();
        let mut sparse = Machine::new(&store_image, &[], 0);
        sparse.registers[10] = STACK_START + 0x700;
        sparse.registers[5] = 0xaabb_ccdd;
        let (_, profile) = engine.run_profiled(&mut sparse, 1);
        assert_eq!(profile.native_retired, 1);
        assert_eq!(profile.generated_guest_register_loads, 2);
        assert_eq!(profile.generated_guest_register_stores, 0);
        assert_eq!(profile.generated_register_cache_read_hits, 0);
        assert_eq!(profile.generated_register_cache_write_hits, 0);
        assert_eq!(profile.native_memory_stores, 1);
        assert_eq!(profile.native_fallthrough_dispatches, 1);
        assert_eq!(profile.native_interpret_one_exits, 0);
        assert_eq!(profile.fallback_stores, 0);
        assert_eq!(profile.fallback_retired, 0);
        assert_eq!(sparse.memory.load_u32(STACK_START + 0x700), 0xaabb_ccdd);

        let mut resident = Machine::new(&store_image, &[], 0);
        let address = STACK_START + 0x780;
        resident.registers[10] = address;
        resident.registers[5] = 0x1122_3344;
        resident.memory.store(address, 4, 0, IMAGE_START).unwrap();
        let (result, profile) = engine.run_profiled(&mut resident, 1);
        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(profile.native_retired, 1);
        assert_eq!(profile.generated_guest_register_loads, 2);
        assert_eq!(profile.generated_guest_register_stores, 0);
        assert_eq!(profile.generated_register_cache_read_hits, 0);
        assert_eq!(profile.generated_register_cache_write_hits, 0);
        assert_eq!(profile.native_memory_loads, 0);
        assert_eq!(profile.native_memory_stores, 1);
        assert_eq!(profile.native_fallthrough_dispatches, 1);
        assert_eq!(profile.native_interpret_one_exits, 0);
        assert_eq!(profile.fallback_stores, 0);
        assert_eq!(profile.fallback_retired, 0);
        assert_eq!(resident.memory.load_u32(address), 0x1122_3344);
    }

    #[cfg(all(
        feature = "profile",
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn profile_executes_multiple_checked_accesses_in_one_region() {
        let image = image_with_code_at(&[lw(5, 10, 0), lw(6, 10, 4), addi(7, 7, 1)], IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);
        let address = STACK_START + 0x800;
        machine.registers[10] = address;
        machine
            .memory
            .store(address, 4, 0x1234_5678, IMAGE_START)
            .unwrap();
        machine
            .memory
            .store(address + 4, 4, 0x89ab_cdef, IMAGE_START)
            .unwrap();

        let (result, profile) = engine.run_profiled(&mut machine, 3);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.registers[5], 0x1234_5678);
        assert_eq!(machine.registers[6], 0x89ab_cdef);
        assert_eq!(machine.registers[7], 1);
        assert_eq!(profile.native_retired, 3);
        assert_eq!(profile.native_invocations, 1);
        assert_eq!(profile.native_dispatches, 1);
        assert_eq!(profile.native_memory_loads, 2);
        assert_eq!(profile.native_fallthrough_dispatches, 1);
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
        assert_eq!(native_profile.generated_guest_register_loads, 1);
        assert_eq!(native_profile.generated_guest_register_stores, 1);
        assert_eq!(native_profile.generated_register_cache_read_hits, 1);
        assert_eq!(native_profile.generated_register_cache_write_hits, 1);
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
        assert_eq!(profile.generated_guest_register_loads, 3);
        assert_eq!(profile.generated_guest_register_stores, 3);
        assert_eq!(profile.generated_register_cache_fills, 0);
        assert_eq!(profile.generated_register_cache_spills, 0);
        assert_eq!(profile.generated_register_cache_read_hits, 0);
        assert_eq!(profile.generated_register_cache_write_hits, 0);
    }

    #[cfg(all(
        feature = "profile",
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn profile_counts_the_singleton_memory_branch_jal_chain() {
        let image = singleton_control_flow_image();
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let load_profile = engine.native.load_profile();
        assert_eq!(load_profile.compiled_blocks, 4);
        assert_eq!(load_profile.native_guest_instructions, 5);
        assert_eq!(load_profile.fallthrough_blocks, 1);
        assert_eq!(load_profile.branch_blocks, 2);
        assert_eq!(load_profile.direct_jump_blocks, 1);

        let mut machine = Machine::new(&image, &[], 0);
        let (result, profile) = engine.run_profiled(&mut machine, 4);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(profile.native_retired, 4);
        assert_eq!(profile.fallback_retired, 0);
        assert_eq!(profile.native_invocations, 1);
        assert_eq!(profile.native_dispatches, 3);
        assert_eq!(profile.native_direct_link_hits, 2);
        assert_eq!(profile.native_missing_exits, 1);
        assert_eq!(profile.native_memory_loads, 1);
        assert_eq!(profile.native_fallthrough_dispatches, 1);
        assert_eq!(profile.native_branch_dispatches, 1);
        assert_eq!(profile.native_direct_jump_dispatches, 1);
    }

    #[cfg(all(
        feature = "profile",
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn profile_retires_every_rv32m_operation_natively() {
        let code = (0..8)
            .map(|funct3| (1 << 25) | (7 << 20) | (6 << 15) | (funct3 << 12) | (5 << 7) | 0x33)
            .collect::<Vec<_>>();
        let image = image_with_code_at(&code, IMAGE_START);
        let mut engine = AotCompiler::default();
        engine.prepare(&image).unwrap();
        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[6] = 0x8000_0001;
        machine.registers[7] = 3;

        let (result, profile) = engine.run_profiled(&mut machine, code.len() as u64);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(profile.native_retired, 8);
        assert_eq!(profile.fallback_retired, 0);
        assert_eq!(profile.fallback_m_ops, 0);
        assert_eq!(profile.generated_guest_register_loads, 3);
        assert_eq!(profile.generated_guest_register_stores, 3);
        assert_eq!(profile.generated_register_cache_read_hits, 16);
        assert_eq!(profile.generated_register_cache_write_hits, 8);
    }
}
