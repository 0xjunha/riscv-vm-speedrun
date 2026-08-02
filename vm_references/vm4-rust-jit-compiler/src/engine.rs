//! Dispatches between cached native code and precise interpreted execution.

use rv32vm_rust_common::{
    machine::{Engine, Machine, RunResult, Termination},
    memory::{ADDRESS_SPACE_SIZE, Image},
};
use rv32vm_rust_x86_block_compiler::{NativeEntry, NativeEntryKind};

#[cfg(feature = "profile")]
use crate::profile::ProfileCounters;
use crate::{
    block::BasicBlock,
    cache::{BlockCache, BlockId, BlockLookup, NativeContinuation, RegionMetadata},
};

const MAX_CODE_BYTES: usize = 16 * 1024 * 1024;
const MAX_LINKED_CONTINUATION_HOPS: usize = 32;

enum CachedExecution {
    NormalExit { source: BlockId, native: bool },
    UnprofiledContinue,
    Terminated(RunResult),
}

#[derive(Clone, Copy)]
enum NativeExecution<'a> {
    Basic { source: BlockId },
    Region { metadata: &'a RegionMetadata },
}

/// Profiles cached blocks and executes their native tier when available.
#[derive(Default)]
pub(crate) struct JitInterpreter {
    cache: BlockCache,
    code_bytes: usize,
    #[cfg(feature = "profile")]
    profile: ProfileCounters,
}

impl JitInterpreter {
    fn execute_block(
        machine: &mut Machine,
        instruction_limit: u64,
        block: &BasicBlock,
        #[cfg(feature = "profile")] profile: &mut ProfileCounters,
    ) -> Option<RunResult> {
        let remaining = instruction_limit - machine.retired;
        let permitted = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(block.len());

        #[cfg(feature = "profile")]
        profile.record_interpreted_block_call();
        for &instruction in &block.instructions()[..permitted] {
            #[cfg(feature = "profile")]
            profile.record_interpreted_attempt(&instruction);
            #[cfg(feature = "profile")]
            let retired_before = machine.retired;
            let termination = machine.execute_one(instruction);
            #[cfg(feature = "profile")]
            profile.record_interpreted_retired(machine.retired - retired_before);
            if let Some(termination) = termination {
                return Some(machine.result(termination));
            }
        }
        (permitted < block.len()).then(|| machine.result(Termination::InstructionLimit))
    }

    fn execute_cached(
        cache: &mut BlockCache,
        id: BlockId,
        machine: &mut Machine,
        instruction_limit: u64,
        code_bytes: &mut usize,
        #[cfg(feature = "profile")] profile: &mut ProfileCounters,
    ) -> CachedExecution {
        let remaining = instruction_limit - machine.retired;
        if cache.staged_revisit_requires_flush(id) {
            let available = MAX_CODE_BYTES.saturating_sub(*code_bytes);
            *code_bytes += cache.flush_pending(
                available,
                #[cfg(feature = "profile")]
                profile,
            );
        }
        if let Some(region) = cache.native_region_entry(id) {
            let logical_count = region.entry.instruction_count();
            let minimum_count = region.entry.minimum_instruction_count();
            if remaining >= minimum_count as u64 {
                return Self::execute_native(
                    region.entry,
                    logical_count,
                    NativeExecution::Region {
                        metadata: region.metadata,
                    },
                    machine,
                    instruction_limit,
                    #[cfg(feature = "profile")]
                    profile,
                );
            }
            #[cfg(feature = "profile")]
            {
                profile.record_region_budget_fallback();
                if region.metadata.is_loop() {
                    profile.record_loop_budget_fallback();
                }
            }
        }

        if let Some(native) = cache.native_entry(id) {
            let maximum_count = native.instruction_count();
            debug_assert!(maximum_count <= cache.block(id).len());
            if remaining < maximum_count as u64 {
                #[cfg(feature = "profile")]
                let result =
                    Self::execute_block(machine, instruction_limit, cache.block(id), profile);
                #[cfg(not(feature = "profile"))]
                let result = Self::execute_block(machine, instruction_limit, cache.block(id));
                return result.map_or(
                    CachedExecution::NormalExit {
                        source: id,
                        native: false,
                    },
                    CachedExecution::Terminated,
                );
            }

            Self::execute_native(
                native,
                maximum_count,
                NativeExecution::Basic { source: id },
                machine,
                instruction_limit,
                #[cfg(feature = "profile")]
                profile,
            )
        } else {
            if remaining < cache.block(id).len() as u64 {
                #[cfg(feature = "profile")]
                let result =
                    Self::execute_block(machine, instruction_limit, cache.block(id), profile);
                #[cfg(not(feature = "profile"))]
                let result = Self::execute_block(machine, instruction_limit, cache.block(id));
                return result.map_or(
                    CachedExecution::NormalExit {
                        source: id,
                        native: false,
                    },
                    CachedExecution::Terminated,
                );
            }

            #[cfg(feature = "profile")]
            let result = Self::execute_block(machine, instruction_limit, cache.block(id), profile);
            #[cfg(not(feature = "profile"))]
            let result = Self::execute_block(machine, instruction_limit, cache.block(id));
            if result.is_none() {
                let available = MAX_CODE_BYTES.saturating_sub(*code_bytes);
                *code_bytes += cache.observe_and_compile(
                    id,
                    available,
                    #[cfg(feature = "profile")]
                    profile,
                );
            }
            result.map_or(
                CachedExecution::NormalExit {
                    source: id,
                    native: false,
                },
                CachedExecution::Terminated,
            )
        }
    }

    fn execute_linked(
        cache: &mut BlockCache,
        id: BlockId,
        machine: &mut Machine,
        instruction_limit: u64,
        code_bytes: &mut usize,
        #[cfg(feature = "profile")] profile: &mut ProfileCounters,
    ) -> CachedExecution {
        let mut continuation_hops = 0;
        let mut execution = Self::execute_cached(
            cache,
            id,
            machine,
            instruction_limit,
            code_bytes,
            #[cfg(feature = "profile")]
            profile,
        );
        loop {
            let CachedExecution::NormalExit {
                source,
                native: true,
            } = &execution
            else {
                #[cfg(feature = "profile")]
                if continuation_hops != 0
                    && matches!(
                        execution,
                        CachedExecution::UnprofiledContinue | CachedExecution::Terminated(_)
                    )
                {
                    profile.record_continuation_non_normal_stop();
                }
                return execution;
            };
            let source = *source;

            #[cfg(feature = "profile")]
            profile.record_continuation_attempt();
            if continuation_hops == MAX_LINKED_CONTINUATION_HOPS {
                #[cfg(feature = "profile")]
                profile.record_continuation_cap_stop();
                return CachedExecution::NormalExit {
                    source,
                    native: true,
                };
            }

            let remaining = instruction_limit.saturating_sub(machine.retired);
            execution = match cache.native_continuation(source, machine.pc, remaining) {
                NativeContinuation::Profiling => {
                    #[cfg(feature = "profile")]
                    profile.record_continuation_profile_stop();
                    return CachedExecution::NormalExit {
                        source,
                        native: true,
                    };
                }
                NativeContinuation::Miss => {
                    #[cfg(feature = "profile")]
                    profile.record_continuation_link_miss();
                    return CachedExecution::NormalExit {
                        source,
                        native: true,
                    };
                }
                NativeContinuation::Unavailable => {
                    #[cfg(feature = "profile")]
                    profile.record_continuation_link_hit();
                    #[cfg(feature = "profile")]
                    profile.record_continuation_target_stop();
                    return CachedExecution::NormalExit {
                        source,
                        native: true,
                    };
                }
                NativeContinuation::Budget => {
                    #[cfg(feature = "profile")]
                    profile.record_continuation_link_hit();
                    #[cfg(feature = "profile")]
                    profile.record_continuation_budget_stop();
                    return CachedExecution::NormalExit {
                        source,
                        native: true,
                    };
                }
                NativeContinuation::Basic {
                    entry,
                    source,
                    region_budget_fallback,
                    region_loop_budget_fallback,
                } => {
                    #[cfg(feature = "profile")]
                    {
                        profile.record_continuation_link_hit();
                        if region_budget_fallback {
                            profile.record_region_budget_fallback();
                        }
                        if region_loop_budget_fallback {
                            profile.record_loop_budget_fallback();
                        }
                    }
                    #[cfg(not(feature = "profile"))]
                    let _ = (region_budget_fallback, region_loop_budget_fallback);
                    continuation_hops += 1;
                    #[cfg(feature = "profile")]
                    profile.record_continuation_hop();
                    let instruction_count = entry.instruction_count();
                    Self::execute_native(
                        entry,
                        instruction_count,
                        NativeExecution::Basic { source },
                        machine,
                        instruction_limit,
                        #[cfg(feature = "profile")]
                        profile,
                    )
                }
                NativeContinuation::Region(region) => {
                    #[cfg(feature = "profile")]
                    profile.record_continuation_link_hit();
                    continuation_hops += 1;
                    #[cfg(feature = "profile")]
                    profile.record_continuation_hop();
                    let instruction_count = region.entry.instruction_count();
                    Self::execute_native(
                        region.entry,
                        instruction_count,
                        NativeExecution::Region {
                            metadata: region.metadata,
                        },
                        machine,
                        instruction_limit,
                        #[cfg(feature = "profile")]
                        profile,
                    )
                }
            };
        }
    }

    fn execute_native(
        native: NativeEntry<'_>,
        entry_instruction_count: usize,
        execution: NativeExecution<'_>,
        machine: &mut Machine,
        instruction_limit: u64,
        #[cfg(feature = "profile")] profile: &mut ProfileCounters,
    ) -> CachedExecution {
        let remaining = instruction_limit - machine.retired;
        let memory = machine.memory.native_view();
        let outcome = native
            .execute_with_limit(&mut machine.registers, memory, remaining)
            .expect("dispatch checks the native entry's minimum budget");
        let retired = outcome.retired() as u64;
        debug_assert!(retired <= remaining);
        debug_assert!(
            native.kind() == NativeEntryKind::Loop || retired <= entry_instruction_count as u64
        );
        debug_assert!(
            matches!(execution, NativeExecution::Region { .. })
                || native.kind() == NativeEntryKind::Bounded
        );
        machine.pc = outcome.next_pc();
        debug_assert_eq!(machine.registers[0], 0);
        machine.retired += retired;
        #[cfg(feature = "profile")]
        {
            profile.record_native_call(retired as usize);
            let (fused_rotates, elided_shifts) = native.optimization_counts(retired as usize);
            profile.record_native_optimizations(fused_rotates, elided_shifts);
            if matches!(execution, NativeExecution::Region { .. }) {
                profile.record_region_call(retired as usize);
            }
            if let NativeExecution::Region { metadata } = execution
                && let Some(cycle_instructions) = metadata.cycle_instructions()
            {
                profile.record_loop_call(retired as usize, cycle_instructions);
            }
        }

        if !outcome.needs_interpreter() {
            return match execution {
                NativeExecution::Basic { source } => CachedExecution::NormalExit {
                    source,
                    native: true,
                },
                NativeExecution::Region { metadata } => {
                    if metadata.is_loop_budget_completion(retired as usize, machine.pc) {
                        #[cfg(feature = "profile")]
                        {
                            profile.record_region_completed_call();
                            profile.record_loop_budget_completion();
                        }
                        return CachedExecution::UnprofiledContinue;
                    }
                    if metadata.is_loop() || retired < entry_instruction_count as u64 {
                        #[cfg(feature = "profile")]
                        profile.record_region_guard_exit();
                    } else {
                        debug_assert_eq!(retired, entry_instruction_count as u64);
                        #[cfg(feature = "profile")]
                        profile.record_region_completed_call();
                        return CachedExecution::NormalExit {
                            source: metadata.final_source(),
                            native: true,
                        };
                    }
                    #[cfg(feature = "profile")]
                    if metadata.is_loop() {
                        profile.record_loop_guard_exit();
                    }
                    CachedExecution::NormalExit {
                        source: metadata
                            .source_for_retired(retired as usize)
                            .expect("normal region guards occur at exact block boundaries"),
                        native: true,
                    }
                }
            };
        }

        #[cfg(feature = "profile")]
        if let NativeExecution::Region { metadata } = execution {
            profile.record_region_side_exit();
            if metadata.is_loop() {
                profile.record_loop_side_exit();
            }
        }
        debug_assert!(machine.retired < instruction_limit);
        let instruction = machine.fetch_decode(machine.pc);
        #[cfg(feature = "profile")]
        profile.record_native_side_exit(&instruction);
        #[cfg(feature = "profile")]
        profile.record_interpreted_block_call();
        #[cfg(feature = "profile")]
        profile.record_interpreted_attempt(&instruction);
        #[cfg(feature = "profile")]
        let retired_before = machine.retired;
        let termination = machine.execute_one(instruction);
        #[cfg(feature = "profile")]
        profile.record_interpreted_retired(machine.retired - retired_before);
        termination.map_or(CachedExecution::UnprofiledContinue, |termination| {
            CachedExecution::Terminated(machine.result(termination))
        })
    }
}

#[cfg(feature = "profile")]
impl Drop for JitInterpreter {
    fn drop(&mut self) {
        self.profile.emit_if_loaded();
    }
}

impl Engine for JitInterpreter {
    fn prepare(&mut self, _image: &Image) -> Result<(), String> {
        #[cfg(feature = "profile")]
        self.profile.start_image();
        self.cache.clear();
        self.code_bytes = 0;
        Ok(())
    }

    fn initialize_direct_memory(&self) -> bool {
        true
    }

    fn run(&mut self, machine: &mut Machine, instruction_limit: u64) -> RunResult {
        #[cfg(feature = "profile")]
        self.profile.begin_run();
        let mut pending_edge = None;
        let result = loop {
            if machine.retired >= instruction_limit {
                break machine.result(Termination::InstructionLimit);
            }

            let pc = machine.pc;
            if pc & 3 != 0 || pc >= ADDRESS_SPACE_SIZE {
                pending_edge = None;
                let instruction = machine.fetch_decode(pc);
                #[cfg(feature = "profile")]
                self.profile.record_interpreted_block_call();
                #[cfg(feature = "profile")]
                self.profile.record_interpreted_attempt(&instruction);
                #[cfg(feature = "profile")]
                let retired_before = machine.retired;
                let termination = machine.execute_one(instruction);
                #[cfg(feature = "profile")]
                self.profile
                    .record_interpreted_retired(machine.retired - retired_before);
                if let Some(termination) = termination {
                    break machine.result(termination);
                }
                continue;
            }

            let lookup = self.cache.get_or_translate(
                machine,
                pc,
                #[cfg(feature = "profile")]
                &mut self.profile,
            );
            match lookup {
                BlockLookup::Cached(id) => {
                    if let Some((source, target_pc)) = pending_edge.take() {
                        debug_assert_eq!(target_pc, pc);
                        let observed = self.cache.observe_edge(
                            source,
                            target_pc,
                            id,
                            #[cfg(feature = "profile")]
                            &mut self.profile,
                        );
                        if observed {
                            let available = MAX_CODE_BYTES.saturating_sub(self.code_bytes);
                            self.code_bytes += self.cache.observe_and_compile_region(
                                source,
                                available,
                                #[cfg(feature = "profile")]
                                &mut self.profile,
                            );
                        }
                    }
                    match Self::execute_linked(
                        &mut self.cache,
                        id,
                        machine,
                        instruction_limit,
                        &mut self.code_bytes,
                        #[cfg(feature = "profile")]
                        &mut self.profile,
                    ) {
                        CachedExecution::NormalExit { source, .. } => {
                            pending_edge = self
                                .cache
                                .profiles_edges(source)
                                .then_some((source, machine.pc));
                        }
                        CachedExecution::UnprofiledContinue => {}
                        CachedExecution::Terminated(result) => break result,
                    }
                }
                BlockLookup::Transient(block) => {
                    pending_edge = None;
                    if let Some(result) = Self::execute_block(
                        machine,
                        instruction_limit,
                        &block,
                        #[cfg(feature = "profile")]
                        &mut self.profile,
                    ) {
                        break result;
                    }
                }
            }
        };
        let available = MAX_CODE_BYTES.saturating_sub(self.code_bytes);
        self.code_bytes += self.cache.flush_pending(
            available,
            #[cfg(feature = "profile")]
            &mut self.profile,
        );
        #[cfg(feature = "profile")]
        self.profile.end_run();
        result
    }
}

#[cfg(test)]
mod tests {
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use rv32vm_rust_common::memory::STACK_END;
    use rv32vm_rust_common::{
        machine::{Engine, Machine, Termination},
        memory::IMAGE_START,
    };
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use rv32vm_rust_x86_block_compiler::NativeEntryKind;

    use super::JitInterpreter;
    #[cfg(all(
        feature = "profile",
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use crate::cache::STAGED_REVISIT_FLUSH_INTERVAL;
    use crate::test_support::{NOP, addi, image_with_code_at, lw, machine_with_code_at};

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    fn long_cycle_code(blocks: usize, load_block: Option<usize>) -> Vec<u32> {
        use crate::test_support::beq;

        assert!(blocks > 8);
        let instruction_count = blocks * 2;
        let mut code = Vec::with_capacity(instruction_count);
        for block in 0..blocks {
            code.push(if load_block == Some(block) {
                lw(6, 7, 0)
            } else {
                addi(5, 5, 1)
            });
            let branch_index = block * 2 + 1;
            let offset = if block + 1 == blocks {
                -i32::try_from(branch_index * 4).unwrap()
            } else {
                4
            };
            code.push(beq(0, 0, offset));
        }
        code
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    fn warm_long_cycle(
        engine: &mut JitInterpreter,
        image: &rv32vm_rust_common::memory::Image,
        load_address: Option<u32>,
    ) {
        for _ in 0..3 {
            let mut machine = Machine::new(image, &[], 0);
            if let Some(address) = load_address {
                machine.registers[7] = address;
            }
            let result = engine.run(&mut machine, 4_096);
            assert_eq!(result.termination, Termination::InstructionLimit);
        }
        assert!(engine.cache.native_region_count() > 0);
    }

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

    #[test]
    fn records_only_normally_dispatched_cached_edges() {
        let image = image_with_code_at(&[0x0000_0063], IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        let mut engine = JitInterpreter::default();

        let result = engine.run(&mut machine, 3);

        assert_eq!(result.termination, Termination::InstructionLimit);
        let source = engine.cache.cached_block_id(IMAGE_START).unwrap();
        let edges = engine.cache.edge_snapshot(source).unwrap();
        assert_eq!(edges.observations, 2);
        assert_eq!(edges.successors[0].unwrap().target, source);
        #[cfg(feature = "profile")]
        {
            assert_eq!(engine.profile.edge_observations(), 2);
            assert_eq!(engine.profile.edge_profile_hits(), 1);
            assert_eq!(engine.profile.edge_profile_replacements(), 0);
        }

        let exit_image = image_with_code_at(&[0x0000_0073], IMAGE_START);
        engine.prepare(&exit_image).unwrap();
        let mut exiting = Machine::new(&exit_image, &[], 0);
        let result = engine.run(&mut exiting, 1);
        assert!(matches!(result.termination, Termination::Exit(_)));
        let exit = engine.cache.cached_block_id(IMAGE_START).unwrap();
        assert_eq!(engine.cache.edge_snapshot(exit).unwrap().observations, 0);
    }

    #[test]
    fn does_not_record_an_edge_to_a_transient_target() {
        use crate::cache::BlockCache;

        let image = image_with_code_at(&[0x0000_0263, 0x0000_0063], IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        let mut engine = JitInterpreter {
            cache: BlockCache::with_limits(1, usize::MAX),
            code_bytes: 0,
            #[cfg(feature = "profile")]
            profile: Default::default(),
        };

        let result = engine.run(&mut machine, 2);

        assert_eq!(result.termination, Termination::InstructionLimit);
        let source = engine.cache.cached_block_id(IMAGE_START).unwrap();
        assert_eq!(engine.cache.edge_snapshot(source).unwrap().observations, 0);
    }

    #[cfg(feature = "profile")]
    #[test]
    fn profile_accounts_runs_retirement_and_cache_dispatch() {
        let image = image_with_code_at(&[addi(5, 5, 1), 0x0000_0063], IMAGE_START);
        let mut engine = JitInterpreter::default();
        engine.prepare(&image).unwrap();

        for _ in 0..2 {
            let mut machine = Machine::new(&image, &[], 0);
            let result = engine.run(&mut machine, 1);
            assert_eq!(result.termination, Termination::InstructionLimit);
        }

        assert_eq!(engine.profile.runs(), 2);
        assert_eq!(engine.profile.native_retired(), 0);
        assert_eq!(engine.profile.interpreted_retired(), 2);
        assert_eq!(engine.profile.interpreted_block_calls(), 2);
        assert_eq!(engine.profile.cache_hits(), 1);
        assert_eq!(engine.profile.cache_misses(), 1);
        assert_eq!(engine.profile.recent_run_count(), 2);
        assert_eq!(engine.profile.most_recent_run_retired(), Some(1));
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
        assert_eq!(engine.cache.program_count(), 1);
        assert_eq!(engine.cache.staged_block_count(), 0);
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
        assert_eq!(engine.cache.program_count(), 0);
        assert_eq!(engine.cache.staged_block_count(), 0);
        assert_eq!(engine.code_bytes, 0);
    }

    #[cfg(all(
        feature = "profile",
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn promotes_a_tight_loop_during_one_long_run() {
        use crate::test_support::beq;

        let image = image_with_code_at(&[addi(5, 5, 1), beq(0, 0, -4)], IMAGE_START);
        let iterations = (3 + STAGED_REVISIT_FLUSH_INTERVAL) as u64;
        let instruction_limit = iterations * 2;
        let mut machine = Machine::new(&image, &[], 0);
        let mut engine = JitInterpreter::default();
        engine.prepare(&image).unwrap();

        let result = engine.run(&mut machine, instruction_limit);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.retired, instruction_limit);
        assert_eq!(u64::from(machine.registers[5]), iterations);
        assert!(engine.profile.native_retired() > 0);
        assert!(engine.profile.interpreted_retired() < instruction_limit);
        assert_eq!(engine.cache.native_block_count(), 1);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn native_memory_uses_flat_fast_paths_without_allocation_side_exits() {
        use crate::test_support::{beq, sw};

        let image = image_with_code_at(
            &[addi(5, 0, 7), sw(5, 2, -4), lw(6, 2, -4), beq(0, 0, -12)],
            IMAGE_START,
        );
        let mut engine = JitInterpreter::default();

        for _ in 0..3 {
            let mut machine = Machine::new(&image, &[], 0);
            let result = engine.run(&mut machine, 4);
            assert_eq!(result.termination, Termination::InstructionLimit);
            assert_eq!(machine.registers[6], 7);
        }
        assert_eq!(engine.cache.native_block_count(), 1);

        let mut fast_path = Machine::new(&image, &[], 0);
        fast_path
            .memory
            .store(STACK_END - 4, 4, 0, IMAGE_START)
            .unwrap();
        let result = engine.run(&mut fast_path, 4);
        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(fast_path.retired, 4);
        assert_eq!(fast_path.registers[6], 7);

        #[cfg(feature = "profile")]
        let native_retired_before = engine.profile.native_retired();
        let mut fresh_stack = Machine::new(&image, &[], 0);
        let result = engine.run(&mut fresh_stack, 4);
        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(fresh_stack.retired, 4);
        assert_eq!(fresh_stack.registers[6], 7);
        assert_eq!(fresh_stack.memory.load_u32(STACK_END - 4), 7);
        #[cfg(feature = "profile")]
        {
            assert_eq!(engine.profile.native_retired(), native_retired_before + 4);
            assert_eq!(engine.profile.native_side_exits(), 0);
            assert_eq!(engine.profile.native_side_exit_opcode_count(0x23), 0);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn native_side_exit_does_not_create_a_source_edge() {
        use crate::test_support::{beq, div};

        let image = image_with_code_at(&[addi(5, 0, 7), div(6, 5, 7), beq(0, 0, -8)], IMAGE_START);
        let mut engine = JitInterpreter::default();
        let mut warming = Machine::new(&image, &[], 0);
        warming.registers[7] = 1;
        let result = engine.run(&mut warming, 9);
        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(engine.cache.native_block_count(), 1);
        let source = engine.cache.cached_block_id(IMAGE_START).unwrap();
        let observations_before = engine.cache.edge_snapshot(source).unwrap().observations;

        let mut side_exit = Machine::new(&image, &[], 0);
        let result = engine.run(&mut side_exit, 3);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(side_exit.registers[6], u32::MAX);
        assert_eq!(
            engine.cache.edge_snapshot(source).unwrap().observations,
            observations_before
        );
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn linked_continuations_preserve_every_exact_limit_and_stop_at_the_hop_cap() {
        let blocks = 10;
        let cycle_instructions = blocks * 2;
        let image = image_with_code_at(&long_cycle_code(blocks, None), IMAGE_START);
        let mut engine = JitInterpreter::default();
        warm_long_cycle(&mut engine, &image, None);
        #[cfg(feature = "profile")]
        let hops_before = engine.profile.continuation_hops();

        for instruction_limit in 0..cycle_instructions as u64 * 4 {
            let mut machine = Machine::new(&image, &[], 0);
            let result = engine.run(&mut machine, instruction_limit);
            let complete_cycles = instruction_limit / cycle_instructions as u64;
            let tail = instruction_limit % cycle_instructions as u64;
            let expected_increments = complete_cycles * blocks as u64 + tail.div_ceil(2);

            assert_eq!(result.termination, Termination::InstructionLimit);
            assert_eq!(machine.retired, instruction_limit);
            assert_eq!(u64::from(machine.registers[5]), expected_increments);
            assert_eq!(machine.pc, IMAGE_START + u32::try_from(tail * 4).unwrap());
        }
        #[cfg(feature = "profile")]
        assert!(engine.profile.continuation_hops() > hops_before);

        #[cfg(feature = "profile")]
        let caps_before = engine.profile.continuation_cap_stops();
        let instruction_limit = (cycle_instructions * 2_000) as u64;
        let mut capped = Machine::new(&image, &[], 0);
        let result = engine.run(&mut capped, instruction_limit);
        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(capped.retired, instruction_limit);
        assert_eq!(capped.registers[5], 20_000);
        assert_eq!(capped.pc, IMAGE_START);
        #[cfg(feature = "profile")]
        assert!(engine.profile.continuation_cap_stops() > caps_before);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn linked_target_fault_preserves_prior_retirement_and_stops_the_chain() {
        let blocks = 10;
        let fault_block = 8;
        let image = image_with_code_at(&long_cycle_code(blocks, Some(fault_block)), IMAGE_START);
        let mut engine = JitInterpreter::default();
        warm_long_cycle(&mut engine, &image, Some(STACK_END - 4));
        #[cfg(feature = "profile")]
        let hops_before = engine.profile.continuation_hops();
        #[cfg(feature = "profile")]
        let stops_before = engine.profile.continuation_non_normal_stops();
        #[cfg(feature = "profile")]
        let exits_before = engine.profile.native_side_exits();

        let mut faulting = Machine::new(&image, &[], 0);
        faulting.registers[7] = 1;
        let result = engine.run(&mut faulting, 100);

        assert!(matches!(result.termination, Termination::Trap(_)));
        assert_eq!(faulting.retired, (fault_block * 2) as u64);
        assert_eq!(faulting.registers[5], fault_block as u32);
        assert_eq!(faulting.registers[6], 0);
        assert_eq!(faulting.pc, IMAGE_START + (fault_block * 8) as u32);
        #[cfg(feature = "profile")]
        {
            assert!(engine.profile.continuation_hops() > hops_before);
            assert_eq!(
                engine.profile.continuation_non_normal_stops(),
                stops_before + 1
            );
            assert_eq!(engine.profile.native_side_exits(), exits_before + 1);
        }
    }

    #[cfg(all(
        feature = "profile",
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn prepare_discards_frozen_continuations_for_same_pc_replacement_image() {
        let image = image_with_code_at(&long_cycle_code(10, None), IMAGE_START);
        let mut engine = JitInterpreter::default();
        engine.prepare(&image).unwrap();
        warm_long_cycle(&mut engine, &image, None);
        let mut steady = Machine::new(&image, &[], 0);
        engine.run(&mut steady, 4_096);
        assert!(engine.profile.continuation_hops() > 0);

        let replacement = image_with_code_at(&[addi(5, 0, 99), 0x0000_0073], IMAGE_START);
        engine.prepare(&replacement).unwrap();
        let mut replaced = Machine::new(&replacement, &[], 0);
        let result = engine.run(&mut replaced, 2);

        assert!(matches!(result.termination, Termination::Exit(_)));
        assert_eq!(replaced.registers[5], 99);
        assert_eq!(replaced.retired, 2);
        assert_eq!(engine.profile.continuation_hops(), 0);
        assert_eq!(engine.profile.continuation_attempts(), 0);
        assert_eq!(engine.cache.block_count(), 1);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn counted_loop_completion_suppresses_edges_then_preserves_exact_basic_tail() {
        use crate::test_support::beq;

        let image = image_with_code_at(
            &[
                addi(5, 5, 1),
                beq(0, 0, 8),
                beq(0, 0, 0),
                addi(6, 6, 1),
                beq(0, 0, -16),
            ],
            IMAGE_START,
        );
        let mut engine = JitInterpreter::default();
        for _ in 0..2 {
            let mut warming = Machine::new(&image, &[], 0);
            let result = engine.run(&mut warming, 64);
            assert_eq!(result.termination, Termination::InstructionLimit);
        }
        assert_eq!(engine.cache.native_region_count(), 2);

        let head = engine.cache.cached_block_id(IMAGE_START).unwrap();
        let successor = engine.cache.cached_block_id(IMAGE_START + 12).unwrap();
        let region = engine.cache.native_region_entry(head).unwrap();
        let region_instructions = region.entry.instruction_count();
        let minimum_instructions = region.entry.minimum_instruction_count();
        assert_eq!(region_instructions, 4);
        assert_eq!(minimum_instructions, 4);
        assert_eq!(region.entry.loop_unroll_factor(), 1);
        assert_eq!(engine.cache.native_loop_count(), 2);
        let head_edges_before = engine.cache.edge_snapshot(head).unwrap().observations;
        let successor_edges_before = engine.cache.edge_snapshot(successor).unwrap().observations;
        #[cfg(feature = "profile")]
        let calls_before = engine.profile.region_calls();
        #[cfg(feature = "profile")]
        let completed_before = engine.profile.region_completed_calls();
        #[cfg(feature = "profile")]
        let budget_before = engine.profile.region_budget_fallbacks();
        #[cfg(feature = "profile")]
        let loop_calls_before = engine.profile.loop_calls();
        #[cfg(feature = "profile")]
        let loop_cycles_before = engine.profile.loop_cycles();
        #[cfg(feature = "profile")]
        let loop_completed_before = engine.profile.loop_budget_completions();
        #[cfg(feature = "profile")]
        let loop_budget_before = engine.profile.loop_budget_fallbacks();
        let mut machine = Machine::new(&image, &[], 0);
        let instruction_limit = (minimum_instructions + 2) as u64;
        let result = engine.run(&mut machine, instruction_limit);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.retired, instruction_limit);
        assert_eq!(machine.registers[5], 2);
        assert_eq!(machine.registers[6], 1);
        assert_eq!(machine.pc, IMAGE_START + 12);
        assert_eq!(
            engine.cache.edge_snapshot(head).unwrap().observations,
            head_edges_before
        );
        assert_eq!(
            engine.cache.edge_snapshot(successor).unwrap().observations,
            successor_edges_before
        );
        #[cfg(feature = "profile")]
        {
            assert_eq!(engine.profile.region_calls(), calls_before + 1);
            assert_eq!(
                engine.profile.region_completed_calls(),
                completed_before + 1
            );
            assert_eq!(engine.profile.region_budget_fallbacks(), budget_before + 1);
            assert_eq!(engine.profile.loop_calls(), loop_calls_before + 1);
            assert_eq!(engine.profile.loop_cycles(), loop_cycles_before + 1);
            assert_eq!(
                engine.profile.loop_budget_completions(),
                loop_completed_before + 1
            );
            assert_eq!(
                engine.profile.loop_budget_fallbacks(),
                loop_budget_before + 1
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
    fn warmed_regions_stop_steady_edge_profiling() {
        use crate::test_support::beq;

        let image = image_with_code_at(
            &[
                addi(5, 5, 1),
                beq(0, 0, 8),
                beq(0, 0, 0),
                addi(6, 6, 1),
                beq(0, 0, -16),
            ],
            IMAGE_START,
        );
        let mut engine = JitInterpreter::default();
        for _ in 0..2 {
            let mut warming = Machine::new(&image, &[], 0);
            let result = engine.run(&mut warming, 64);
            assert_eq!(result.termination, Termination::InstructionLimit);
        }
        assert_eq!(engine.cache.native_region_count(), 2);
        assert_eq!(engine.cache.native_loop_count(), 2);
        let observations_before = engine.profile.edge_observations();
        let calls_before = engine.profile.region_calls();
        let loop_calls_before = engine.profile.loop_calls();

        let mut steady = Machine::new(&image, &[], 0);
        let result = engine.run(&mut steady, 64);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(steady.retired, 64);
        assert_eq!(engine.profile.edge_observations(), observations_before);
        assert!(engine.profile.region_calls() > calls_before);
        assert!(engine.profile.loop_calls() > loop_calls_before);
    }

    #[cfg(all(
        feature = "profile",
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn warmed_self_loop_regions_count_cycles_and_stop_steady_edge_profiling() {
        use crate::test_support::beq;

        let image = image_with_code_at(&[addi(5, 5, 1), beq(0, 0, -4)], IMAGE_START);
        let mut engine = JitInterpreter::default();
        for _ in 0..2 {
            let mut warming = Machine::new(&image, &[], 0);
            let result = engine.run(&mut warming, 64);
            assert_eq!(result.termination, Termination::InstructionLimit);
        }
        let head = engine.cache.cached_block_id(IMAGE_START).unwrap();
        assert!(!engine.cache.profiles_edges(head));
        assert_eq!(engine.cache.native_region_count(), 1);
        assert_eq!(engine.cache.native_loop_count(), 1);
        let region = engine.cache.native_region_entry(head).unwrap();
        assert_eq!(region.entry.kind(), NativeEntryKind::Loop);
        assert_eq!(region.metadata.block_count(), 1);
        assert_eq!(region.entry.instruction_count(), 2);
        assert_eq!(region.entry.minimum_instruction_count(), 2);
        assert_eq!(region.entry.loop_unroll_factor(), 1);
        let observations_before = engine.cache.edge_snapshot(head).unwrap().observations;
        let aggregate_before = engine.profile.edge_observations();
        let calls_before = engine.profile.region_calls();
        let loop_calls_before = engine.profile.loop_calls();
        let loop_cycles_before = engine.profile.loop_cycles();
        let loop_completed_before = engine.profile.loop_budget_completions();

        let mut steady = Machine::new(&image, &[], 0);
        let result = engine.run(&mut steady, 64);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(steady.retired, 64);
        assert_eq!(
            engine.cache.edge_snapshot(head).unwrap().observations,
            observations_before
        );
        assert_eq!(engine.profile.edge_observations(), aggregate_before);
        assert_eq!(engine.profile.region_calls(), calls_before + 1);
        assert_eq!(engine.profile.loop_calls(), loop_calls_before + 1);
        assert_eq!(engine.profile.loop_cycles(), loop_cycles_before + 32);
        assert_eq!(
            engine.profile.loop_budget_completions(),
            loop_completed_before + 1
        );
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn counted_self_loop_preserves_every_short_tail_budget() {
        use crate::test_support::beq;

        let image = image_with_code_at(&[addi(5, 5, 1), beq(0, 0, -4)], IMAGE_START);
        let mut engine = JitInterpreter::default();
        for _ in 0..2 {
            let mut warming = Machine::new(&image, &[], 0);
            let result = engine.run(&mut warming, 64);
            assert_eq!(result.termination, Termination::InstructionLimit);
        }
        let head = engine.cache.cached_block_id(IMAGE_START).unwrap();
        let region = engine.cache.native_region_entry(head).unwrap();
        assert_eq!(region.entry.kind(), NativeEntryKind::Loop);
        assert_eq!(region.entry.instruction_count(), 2);
        assert_eq!(region.entry.minimum_instruction_count(), 2);
        assert_eq!(region.entry.loop_unroll_factor(), 1);
        let minimum_instructions = region.entry.minimum_instruction_count();
        let observations_before = engine.cache.edge_snapshot(head).unwrap().observations;
        #[cfg(feature = "profile")]
        let loop_calls_before = engine.profile.loop_calls();
        #[cfg(feature = "profile")]
        let loop_cycles_before = engine.profile.loop_cycles();
        #[cfg(feature = "profile")]
        let loop_retired_before = engine.profile.loop_retired();
        #[cfg(feature = "profile")]
        let loop_completed_before = engine.profile.loop_budget_completions();
        for tail in 0..minimum_instructions {
            let instruction_limit = (minimum_instructions + tail) as u64;
            let mut machine = Machine::new(&image, &[], 0);
            let result = engine.run(&mut machine, instruction_limit);

            assert_eq!(result.termination, Termination::InstructionLimit);
            assert_eq!(machine.retired, instruction_limit);
            assert_eq!(machine.registers[5], 1 + tail as u32);
            assert_eq!(machine.pc, IMAGE_START + (tail % 2) as u32 * 4);
        }
        assert_eq!(
            engine.cache.edge_snapshot(head).unwrap().observations,
            observations_before
        );
        #[cfg(feature = "profile")]
        {
            assert_eq!(engine.profile.loop_calls(), loop_calls_before + 2);
            assert_eq!(engine.profile.loop_cycles(), loop_cycles_before + 2);
            assert_eq!(engine.profile.loop_retired(), loop_retired_before + 4);
            assert_eq!(
                engine.profile.loop_budget_completions(),
                loop_completed_before + 2
            );
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn counted_loop_cycle_boundary_at_a_nonhead_pc_remains_a_guard_exit() {
        use super::{CachedExecution, NativeExecution};
        use crate::test_support::{beq, bne};

        let image = image_with_code_at(&[addi(6, 6, -1), bne(6, 0, -4), beq(0, 0, 0)], IMAGE_START);
        let mut engine = JitInterpreter::default();
        for _ in 0..2 {
            let mut warming = Machine::new(&image, &[], 0);
            warming.registers[6] = 1_000;
            let result = engine.run(&mut warming, 64);
            assert_eq!(result.termination, Termination::InstructionLimit);
        }

        let head = engine.cache.cached_block_id(IMAGE_START).unwrap();
        let region = engine.cache.native_region_entry(head).unwrap();
        assert_eq!(region.entry.kind(), NativeEntryKind::Loop);
        assert_eq!(region.entry.instruction_count(), 2);
        assert_eq!(region.entry.minimum_instruction_count(), 2);
        assert_eq!(region.entry.loop_unroll_factor(), 1);
        #[cfg(feature = "profile")]
        let guards_before = engine.profile.region_guard_exits();
        #[cfg(feature = "profile")]
        let loop_guards_before = engine.profile.loop_guard_exits();
        #[cfg(feature = "profile")]
        let completed_before = engine.profile.loop_budget_completions();
        #[cfg(feature = "profile")]
        let loop_cycles_before = engine.profile.loop_cycles();

        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[6] = 4;
        let execution = JitInterpreter::execute_native(
            region.entry,
            region.entry.instruction_count(),
            NativeExecution::Region {
                metadata: region.metadata,
            },
            &mut machine,
            u64::MAX,
            #[cfg(feature = "profile")]
            &mut engine.profile,
        );

        assert_eq!(machine.retired, 8);
        assert_eq!(machine.registers[6], 0);
        assert_eq!(machine.pc, IMAGE_START + 8);
        assert!(matches!(
            execution,
            CachedExecution::NormalExit { source, native: true } if source == head
        ));
        #[cfg(feature = "profile")]
        {
            assert_eq!(engine.profile.region_guard_exits(), guards_before + 1);
            assert_eq!(engine.profile.loop_guard_exits(), loop_guards_before + 1);
            assert_eq!(engine.profile.loop_budget_completions(), completed_before);
            assert_eq!(engine.profile.loop_cycles(), loop_cycles_before + 4);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn later_block_region_guard_exit_uses_the_exact_source_boundary() {
        use super::{CachedExecution, NativeExecution};
        use crate::test_support::beq;

        let image = image_with_code_at(
            &[
                addi(5, 5, 1),
                beq(0, 0, 8),
                beq(0, 0, 0),
                beq(6, 0, -12),
                beq(0, 0, 0),
            ],
            IMAGE_START,
        );
        let mut engine = JitInterpreter::default();
        for _ in 0..2 {
            let mut warming = Machine::new(&image, &[], 0);
            let result = engine.run(&mut warming, 60);
            assert_eq!(result.termination, Termination::InstructionLimit);
        }
        assert!(engine.cache.native_region_count() >= 1);

        let head = engine.cache.cached_block_id(IMAGE_START).unwrap();
        let successor = engine.cache.cached_block_id(IMAGE_START + 12).unwrap();
        let head_edges_before = engine.cache.edge_snapshot(head).unwrap().observations;
        let successor_edges_before = engine.cache.edge_snapshot(successor).unwrap().observations;
        let region = engine.cache.native_region_entry(head).unwrap();
        assert_eq!(region.entry.kind(), NativeEntryKind::Loop);
        assert_eq!(region.entry.instruction_count(), 3);
        assert_eq!(region.metadata.source_for_retired(3), Some(successor));
        #[cfg(feature = "profile")]
        let calls_before = engine.profile.region_calls();
        #[cfg(feature = "profile")]
        let guards_before = engine.profile.region_guard_exits();
        #[cfg(feature = "profile")]
        let loop_guards_before = engine.profile.loop_guard_exits();

        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[6] = 1;
        let execution = JitInterpreter::execute_native(
            region.entry,
            region.entry.instruction_count(),
            NativeExecution::Region {
                metadata: region.metadata,
            },
            &mut machine,
            u64::MAX,
            #[cfg(feature = "profile")]
            &mut engine.profile,
        );

        assert_eq!(machine.retired, 3);
        assert_eq!(machine.registers[5], 1);
        assert_eq!(machine.pc, IMAGE_START + 16);
        assert!(matches!(
            execution,
            CachedExecution::NormalExit { source, native: true } if source == successor
        ));
        assert_eq!(
            engine.cache.edge_snapshot(head).unwrap().observations,
            head_edges_before
        );
        assert_eq!(
            engine.cache.edge_snapshot(successor).unwrap().observations,
            successor_edges_before
        );
        #[cfg(feature = "profile")]
        {
            assert_eq!(engine.profile.region_calls(), calls_before + 1);
            assert_eq!(engine.profile.region_guard_exits(), guards_before + 1);
            assert_eq!(engine.profile.loop_guard_exits(), loop_guards_before + 1);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn later_counted_loop_side_exit_executes_exactly_one_interpreter_instruction() {
        use super::{CachedExecution, NativeExecution};
        use crate::test_support::{beq, bne, div};

        let image = image_with_code_at(
            &[addi(6, 6, -1), div(7, 5, 6), bne(6, 0, -8), beq(0, 0, 0)],
            IMAGE_START,
        );
        let mut engine = JitInterpreter::default();
        for _ in 0..2 {
            let mut warming = Machine::new(&image, &[], 0);
            warming.registers[5] = 7;
            warming.registers[6] = 1_000;
            let result = engine.run(&mut warming, 96);
            assert_eq!(result.termination, Termination::InstructionLimit);
        }

        let head = engine.cache.cached_block_id(IMAGE_START).unwrap();
        let observations_before = engine.cache.edge_snapshot(head).unwrap().observations;
        let region = engine.cache.native_region_entry(head).unwrap();
        assert_eq!(region.entry.kind(), NativeEntryKind::Loop);
        assert_eq!(region.entry.instruction_count(), 3);
        assert_eq!(region.entry.minimum_instruction_count(), 3);
        assert_eq!(region.entry.loop_unroll_factor(), 1);
        #[cfg(feature = "profile")]
        let calls_before = engine.profile.region_calls();
        #[cfg(feature = "profile")]
        let side_exits_before = engine.profile.region_side_exits();
        #[cfg(feature = "profile")]
        let loop_side_exits_before = engine.profile.loop_side_exits();
        #[cfg(feature = "profile")]
        let loop_cycles_before = engine.profile.loop_cycles();
        #[cfg(feature = "profile")]
        let interpreted_before = engine.profile.interpreted_retired();

        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[5] = 7;
        machine.registers[6] = 4;
        let execution = JitInterpreter::execute_native(
            region.entry,
            region.entry.instruction_count(),
            NativeExecution::Region {
                metadata: region.metadata,
            },
            &mut machine,
            u64::MAX,
            #[cfg(feature = "profile")]
            &mut engine.profile,
        );

        assert!(matches!(execution, CachedExecution::UnprofiledContinue));
        assert_eq!(machine.retired, 11);
        assert_eq!(machine.registers[6], 0);
        assert_eq!(machine.registers[7], u32::MAX);
        assert_eq!(machine.pc, IMAGE_START + 8);
        assert_eq!(
            engine.cache.edge_snapshot(head).unwrap().observations,
            observations_before
        );
        #[cfg(feature = "profile")]
        {
            assert_eq!(engine.profile.region_calls(), calls_before + 1);
            assert_eq!(engine.profile.region_side_exits(), side_exits_before + 1);
            assert_eq!(engine.profile.loop_side_exits(), loop_side_exits_before + 1);
            assert_eq!(engine.profile.loop_cycles(), loop_cycles_before + 3);
            assert_eq!(engine.profile.interpreted_retired(), interpreted_before + 1);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn region_side_exit_at_a_prior_boundary_remains_unprofiled() {
        use super::{CachedExecution, NativeExecution};
        use crate::test_support::{beq, div};

        let image = image_with_code_at(
            &[beq(0, 0, 8), beq(0, 0, 0), div(7, 5, 6), beq(0, 0, -12)],
            IMAGE_START,
        );
        let mut engine = JitInterpreter::default();
        for _ in 0..2 {
            let mut warming = Machine::new(&image, &[], 0);
            warming.registers[6] = 1;
            let result = engine.run(&mut warming, 64);
            assert_eq!(result.termination, Termination::InstructionLimit);
        }
        assert!(engine.cache.native_region_count() >= 1);

        let head = engine.cache.cached_block_id(IMAGE_START).unwrap();
        let head_edges_before = engine.cache.edge_snapshot(head).unwrap().observations;
        let region = engine.cache.native_region_entry(head).unwrap();
        assert_eq!(region.entry.kind(), NativeEntryKind::Loop);
        assert_eq!(region.entry.instruction_count(), 3);
        assert_eq!(region.metadata.source_for_retired(1), Some(head));
        #[cfg(feature = "profile")]
        let calls_before = engine.profile.region_calls();
        #[cfg(feature = "profile")]
        let side_exits_before = engine.profile.region_side_exits();
        #[cfg(feature = "profile")]
        let loop_side_exits_before = engine.profile.loop_side_exits();

        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[5] = 7;
        let execution = JitInterpreter::execute_native(
            region.entry,
            region.entry.instruction_count(),
            NativeExecution::Region {
                metadata: region.metadata,
            },
            &mut machine,
            u64::MAX,
            #[cfg(feature = "profile")]
            &mut engine.profile,
        );

        assert!(matches!(execution, CachedExecution::UnprofiledContinue));
        assert_eq!(machine.retired, 2);
        assert_eq!(machine.registers[5], 7);
        assert_eq!(machine.registers[7], u32::MAX);
        assert_eq!(machine.pc, IMAGE_START + 12);
        assert_eq!(
            engine.cache.edge_snapshot(head).unwrap().observations,
            head_edges_before
        );
        #[cfg(feature = "profile")]
        {
            assert_eq!(engine.profile.region_calls(), calls_before + 1);
            assert_eq!(engine.profile.region_side_exits(), side_exits_before + 1);
            assert_eq!(engine.profile.loop_side_exits(), loop_side_exits_before + 1);
        }
    }
}
