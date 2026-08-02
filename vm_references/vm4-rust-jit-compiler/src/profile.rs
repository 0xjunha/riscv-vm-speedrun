//! Feature-gated aggregate diagnostics for one loaded image.

use std::{collections::VecDeque, fmt::Write as _, io::Write as _, time::Duration};

use crate::block::BlockInstruction;

const OPCODE_COUNT: usize = 128;
const RUN_HISTORY_CAPACITY: usize = 64;

pub(crate) enum CompileFailure {
    NoCode,
    TooShort,
    Publication,
}

#[derive(Clone, Copy, Default)]
struct RunMetrics {
    native_retired: u64,
    interpreted_retired: u64,
    native_calls: u64,
    native_side_exits: u64,
    region_retired: u64,
    region_calls: u64,
    region_completed_calls: u64,
    region_guard_exits: u64,
    region_side_exits: u64,
    region_budget_fallbacks: u64,
    loop_retired: u64,
    loop_calls: u64,
    loop_cycles: u64,
    loop_budget_completions: u64,
    loop_guard_exits: u64,
    loop_side_exits: u64,
    loop_budget_fallbacks: u64,
    interpreted_block_calls: u64,
    interpreted_instructions: u64,
    interpreted_fetch_traps: u64,
    cache_hits: u64,
    cache_misses: u64,
    transient_translations: u64,
    edge_observations: u64,
    edge_profile_hits: u64,
    edge_profile_replacements: u64,
    dominant_edge_observations: u64,
    compile_attempts: u64,
    compile_successes: u64,
    compile_failures: u64,
    compile_no_code_failures: u64,
    compile_too_short_failures: u64,
    compile_publish_failures: u64,
    compiled_code_bytes: u64,
    region_compile_attempts: u64,
    region_compile_successes: u64,
    region_compile_failures: u64,
    region_compiled_code_bytes: u64,
    region_paths_selected: u64,
    region_selected_blocks: u64,
    region_selected_instructions: u64,
    region_compiled_blocks: u64,
    region_compiled_instructions: u64,
    region_path_prefix_fallbacks: u64,
    region_path_block_limit_stops: u64,
    region_path_instruction_limit_stops: u64,
    region_path_terminal_stops: u64,
    region_path_jalr_stops: u64,
    region_path_profile_stops: u64,
    region_path_loop_closures: u64,
    loop_compile_attempts: u64,
    loop_compile_successes: u64,
    loop_compile_failures: u64,
    loop_compiled_code_bytes: u64,
    mapped_code_bytes: u64,
    compile_elapsed_ns: u64,
}

impl RunMetrics {
    fn capture(profile: &ProfileCounters) -> Self {
        Self {
            native_retired: profile.native_retired,
            interpreted_retired: profile.interpreted_retired,
            native_calls: profile.native_calls,
            native_side_exits: profile.native_side_exits,
            region_retired: profile.region_retired,
            region_calls: profile.region_calls,
            region_completed_calls: profile.region_completed_calls,
            region_guard_exits: profile.region_guard_exits,
            region_side_exits: profile.region_side_exits,
            region_budget_fallbacks: profile.region_budget_fallbacks,
            loop_retired: profile.loop_retired,
            loop_calls: profile.loop_calls,
            loop_cycles: profile.loop_cycles,
            loop_budget_completions: profile.loop_budget_completions,
            loop_guard_exits: profile.loop_guard_exits,
            loop_side_exits: profile.loop_side_exits,
            loop_budget_fallbacks: profile.loop_budget_fallbacks,
            interpreted_block_calls: profile.interpreted_block_calls,
            interpreted_instructions: profile.interpreted_instructions,
            interpreted_fetch_traps: profile.interpreted_fetch_traps,
            cache_hits: profile.cache_hits,
            cache_misses: profile.cache_misses,
            transient_translations: profile.transient_translations,
            edge_observations: profile.edge_observations,
            edge_profile_hits: profile.edge_profile_hits,
            edge_profile_replacements: profile.edge_profile_replacements,
            dominant_edge_observations: profile.dominant_edge_observations,
            compile_attempts: profile.compile_attempts,
            compile_successes: profile.compile_successes,
            compile_failures: profile.compile_failures,
            compile_no_code_failures: profile.compile_no_code_failures,
            compile_too_short_failures: profile.compile_too_short_failures,
            compile_publish_failures: profile.compile_publish_failures,
            compiled_code_bytes: profile.compiled_code_bytes,
            region_compile_attempts: profile.region_compile_attempts,
            region_compile_successes: profile.region_compile_successes,
            region_compile_failures: profile.region_compile_failures,
            region_compiled_code_bytes: profile.region_compiled_code_bytes,
            region_paths_selected: profile.region_paths_selected,
            region_selected_blocks: profile.region_selected_blocks,
            region_selected_instructions: profile.region_selected_instructions,
            region_compiled_blocks: profile.region_compiled_blocks,
            region_compiled_instructions: profile.region_compiled_instructions,
            region_path_prefix_fallbacks: profile.region_path_prefix_fallbacks,
            region_path_block_limit_stops: profile.region_path_block_limit_stops,
            region_path_instruction_limit_stops: profile.region_path_instruction_limit_stops,
            region_path_terminal_stops: profile.region_path_terminal_stops,
            region_path_jalr_stops: profile.region_path_jalr_stops,
            region_path_profile_stops: profile.region_path_profile_stops,
            region_path_loop_closures: profile.region_path_loop_closures,
            loop_compile_attempts: profile.loop_compile_attempts,
            loop_compile_successes: profile.loop_compile_successes,
            loop_compile_failures: profile.loop_compile_failures,
            loop_compiled_code_bytes: profile.loop_compiled_code_bytes,
            mapped_code_bytes: profile.mapped_code_bytes,
            compile_elapsed_ns: profile.compile_elapsed_ns,
        }
    }

    fn since(self, earlier: Self) -> Self {
        Self {
            native_retired: self.native_retired.saturating_sub(earlier.native_retired),
            interpreted_retired: self
                .interpreted_retired
                .saturating_sub(earlier.interpreted_retired),
            native_calls: self.native_calls.saturating_sub(earlier.native_calls),
            native_side_exits: self
                .native_side_exits
                .saturating_sub(earlier.native_side_exits),
            region_retired: self.region_retired.saturating_sub(earlier.region_retired),
            region_calls: self.region_calls.saturating_sub(earlier.region_calls),
            region_completed_calls: self
                .region_completed_calls
                .saturating_sub(earlier.region_completed_calls),
            region_guard_exits: self
                .region_guard_exits
                .saturating_sub(earlier.region_guard_exits),
            region_side_exits: self
                .region_side_exits
                .saturating_sub(earlier.region_side_exits),
            region_budget_fallbacks: self
                .region_budget_fallbacks
                .saturating_sub(earlier.region_budget_fallbacks),
            loop_retired: self.loop_retired.saturating_sub(earlier.loop_retired),
            loop_calls: self.loop_calls.saturating_sub(earlier.loop_calls),
            loop_cycles: self.loop_cycles.saturating_sub(earlier.loop_cycles),
            loop_budget_completions: self
                .loop_budget_completions
                .saturating_sub(earlier.loop_budget_completions),
            loop_guard_exits: self
                .loop_guard_exits
                .saturating_sub(earlier.loop_guard_exits),
            loop_side_exits: self.loop_side_exits.saturating_sub(earlier.loop_side_exits),
            loop_budget_fallbacks: self
                .loop_budget_fallbacks
                .saturating_sub(earlier.loop_budget_fallbacks),
            interpreted_block_calls: self
                .interpreted_block_calls
                .saturating_sub(earlier.interpreted_block_calls),
            interpreted_instructions: self
                .interpreted_instructions
                .saturating_sub(earlier.interpreted_instructions),
            interpreted_fetch_traps: self
                .interpreted_fetch_traps
                .saturating_sub(earlier.interpreted_fetch_traps),
            cache_hits: self.cache_hits.saturating_sub(earlier.cache_hits),
            cache_misses: self.cache_misses.saturating_sub(earlier.cache_misses),
            transient_translations: self
                .transient_translations
                .saturating_sub(earlier.transient_translations),
            edge_observations: self
                .edge_observations
                .saturating_sub(earlier.edge_observations),
            edge_profile_hits: self
                .edge_profile_hits
                .saturating_sub(earlier.edge_profile_hits),
            edge_profile_replacements: self
                .edge_profile_replacements
                .saturating_sub(earlier.edge_profile_replacements),
            dominant_edge_observations: self
                .dominant_edge_observations
                .saturating_sub(earlier.dominant_edge_observations),
            compile_attempts: self
                .compile_attempts
                .saturating_sub(earlier.compile_attempts),
            compile_successes: self
                .compile_successes
                .saturating_sub(earlier.compile_successes),
            compile_failures: self
                .compile_failures
                .saturating_sub(earlier.compile_failures),
            compile_no_code_failures: self
                .compile_no_code_failures
                .saturating_sub(earlier.compile_no_code_failures),
            compile_too_short_failures: self
                .compile_too_short_failures
                .saturating_sub(earlier.compile_too_short_failures),
            compile_publish_failures: self
                .compile_publish_failures
                .saturating_sub(earlier.compile_publish_failures),
            compiled_code_bytes: self
                .compiled_code_bytes
                .saturating_sub(earlier.compiled_code_bytes),
            region_compile_attempts: self
                .region_compile_attempts
                .saturating_sub(earlier.region_compile_attempts),
            region_compile_successes: self
                .region_compile_successes
                .saturating_sub(earlier.region_compile_successes),
            region_compile_failures: self
                .region_compile_failures
                .saturating_sub(earlier.region_compile_failures),
            region_compiled_code_bytes: self
                .region_compiled_code_bytes
                .saturating_sub(earlier.region_compiled_code_bytes),
            region_paths_selected: self
                .region_paths_selected
                .saturating_sub(earlier.region_paths_selected),
            region_selected_blocks: self
                .region_selected_blocks
                .saturating_sub(earlier.region_selected_blocks),
            region_selected_instructions: self
                .region_selected_instructions
                .saturating_sub(earlier.region_selected_instructions),
            region_compiled_blocks: self
                .region_compiled_blocks
                .saturating_sub(earlier.region_compiled_blocks),
            region_compiled_instructions: self
                .region_compiled_instructions
                .saturating_sub(earlier.region_compiled_instructions),
            region_path_prefix_fallbacks: self
                .region_path_prefix_fallbacks
                .saturating_sub(earlier.region_path_prefix_fallbacks),
            region_path_block_limit_stops: self
                .region_path_block_limit_stops
                .saturating_sub(earlier.region_path_block_limit_stops),
            region_path_instruction_limit_stops: self
                .region_path_instruction_limit_stops
                .saturating_sub(earlier.region_path_instruction_limit_stops),
            region_path_terminal_stops: self
                .region_path_terminal_stops
                .saturating_sub(earlier.region_path_terminal_stops),
            region_path_jalr_stops: self
                .region_path_jalr_stops
                .saturating_sub(earlier.region_path_jalr_stops),
            region_path_profile_stops: self
                .region_path_profile_stops
                .saturating_sub(earlier.region_path_profile_stops),
            region_path_loop_closures: self
                .region_path_loop_closures
                .saturating_sub(earlier.region_path_loop_closures),
            loop_compile_attempts: self
                .loop_compile_attempts
                .saturating_sub(earlier.loop_compile_attempts),
            loop_compile_successes: self
                .loop_compile_successes
                .saturating_sub(earlier.loop_compile_successes),
            loop_compile_failures: self
                .loop_compile_failures
                .saturating_sub(earlier.loop_compile_failures),
            loop_compiled_code_bytes: self
                .loop_compiled_code_bytes
                .saturating_sub(earlier.loop_compiled_code_bytes),
            mapped_code_bytes: self
                .mapped_code_bytes
                .saturating_sub(earlier.mapped_code_bytes),
            compile_elapsed_ns: self
                .compile_elapsed_ns
                .saturating_sub(earlier.compile_elapsed_ns),
        }
    }
}

struct RunSummary {
    run: u64,
    metrics: RunMetrics,
}

/// Counters collected while one image is loaded.
///
/// The entire type and every call site are compiled out unless the `profile`
/// feature is enabled.
pub(crate) struct ProfileCounters {
    loaded: bool,
    runs: u64,
    native_retired: u64,
    interpreted_retired: u64,
    native_calls: u64,
    native_side_exits: u64,
    region_retired: u64,
    region_calls: u64,
    region_completed_calls: u64,
    region_guard_exits: u64,
    region_side_exits: u64,
    region_budget_fallbacks: u64,
    loop_retired: u64,
    loop_calls: u64,
    loop_cycles: u64,
    loop_budget_completions: u64,
    loop_guard_exits: u64,
    loop_side_exits: u64,
    loop_budget_fallbacks: u64,
    interpreted_block_calls: u64,
    interpreted_instructions: u64,
    interpreted_fetch_traps: u64,
    cache_hits: u64,
    cache_misses: u64,
    transient_translations: u64,
    edge_observations: u64,
    edge_profile_hits: u64,
    edge_profile_replacements: u64,
    dominant_edge_observations: u64,
    compile_attempts: u64,
    compile_successes: u64,
    compile_failures: u64,
    compile_no_code_failures: u64,
    compile_too_short_failures: u64,
    compile_publish_failures: u64,
    compiled_code_bytes: u64,
    region_compile_attempts: u64,
    region_compile_successes: u64,
    region_compile_failures: u64,
    region_compiled_code_bytes: u64,
    region_paths_selected: u64,
    region_selected_blocks: u64,
    region_selected_instructions: u64,
    region_compiled_blocks: u64,
    region_compiled_instructions: u64,
    region_path_prefix_fallbacks: u64,
    region_path_block_limit_stops: u64,
    region_path_instruction_limit_stops: u64,
    region_path_terminal_stops: u64,
    region_path_jalr_stops: u64,
    region_path_profile_stops: u64,
    region_path_loop_closures: u64,
    loop_compile_attempts: u64,
    loop_compile_successes: u64,
    loop_compile_failures: u64,
    loop_compiled_code_bytes: u64,
    mapped_code_bytes: u64,
    compile_elapsed_ns: u64,
    interpreted_opcodes: [u64; OPCODE_COUNT],
    fallback_opcodes: [u64; OPCODE_COUNT],
    native_side_exit_opcodes: [u64; OPCODE_COUNT],
    current_run: Option<(u64, RunMetrics)>,
    recent_runs: VecDeque<RunSummary>,
}

impl Default for ProfileCounters {
    fn default() -> Self {
        Self {
            loaded: false,
            runs: 0,
            native_retired: 0,
            interpreted_retired: 0,
            native_calls: 0,
            native_side_exits: 0,
            region_retired: 0,
            region_calls: 0,
            region_completed_calls: 0,
            region_guard_exits: 0,
            region_side_exits: 0,
            region_budget_fallbacks: 0,
            loop_retired: 0,
            loop_calls: 0,
            loop_cycles: 0,
            loop_budget_completions: 0,
            loop_guard_exits: 0,
            loop_side_exits: 0,
            loop_budget_fallbacks: 0,
            interpreted_block_calls: 0,
            interpreted_instructions: 0,
            interpreted_fetch_traps: 0,
            cache_hits: 0,
            cache_misses: 0,
            transient_translations: 0,
            edge_observations: 0,
            edge_profile_hits: 0,
            edge_profile_replacements: 0,
            dominant_edge_observations: 0,
            compile_attempts: 0,
            compile_successes: 0,
            compile_failures: 0,
            compile_no_code_failures: 0,
            compile_too_short_failures: 0,
            compile_publish_failures: 0,
            compiled_code_bytes: 0,
            region_compile_attempts: 0,
            region_compile_successes: 0,
            region_compile_failures: 0,
            region_compiled_code_bytes: 0,
            region_paths_selected: 0,
            region_selected_blocks: 0,
            region_selected_instructions: 0,
            region_compiled_blocks: 0,
            region_compiled_instructions: 0,
            region_path_prefix_fallbacks: 0,
            region_path_block_limit_stops: 0,
            region_path_instruction_limit_stops: 0,
            region_path_terminal_stops: 0,
            region_path_jalr_stops: 0,
            region_path_profile_stops: 0,
            region_path_loop_closures: 0,
            loop_compile_attempts: 0,
            loop_compile_successes: 0,
            loop_compile_failures: 0,
            loop_compiled_code_bytes: 0,
            mapped_code_bytes: 0,
            compile_elapsed_ns: 0,
            interpreted_opcodes: [0; OPCODE_COUNT],
            fallback_opcodes: [0; OPCODE_COUNT],
            native_side_exit_opcodes: [0; OPCODE_COUNT],
            current_run: None,
            recent_runs: VecDeque::with_capacity(RUN_HISTORY_CAPACITY),
        }
    }
}

impl ProfileCounters {
    pub(crate) fn start_image(&mut self) {
        self.emit_if_loaded();
        *self = Self {
            loaded: true,
            ..Self::default()
        };
    }

    pub(crate) fn emit_if_loaded(&self) {
        if self.loaded {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "{}", self.json());
        }
    }

    pub(crate) fn begin_run(&mut self) {
        increment(&mut self.runs, 1);
        self.current_run = Some((self.runs, RunMetrics::capture(self)));
    }

    pub(crate) fn end_run(&mut self) {
        let Some((run, start)) = self.current_run.take() else {
            return;
        };
        let metrics = RunMetrics::capture(self).since(start);
        if self.recent_runs.len() == RUN_HISTORY_CAPACITY {
            self.recent_runs.pop_front();
        }
        self.recent_runs.push_back(RunSummary { run, metrics });
    }

    pub(crate) fn record_native_call(&mut self, retired: usize) {
        increment(&mut self.native_calls, 1);
        increment(&mut self.native_retired, as_u64(retired));
    }

    pub(crate) fn record_native_side_exit(&mut self, instruction: &BlockInstruction) {
        increment(&mut self.native_side_exits, 1);
        if let Ok(decoded) = instruction {
            increment(
                &mut self.native_side_exit_opcodes[decoded.opcode() as usize],
                1,
            );
        }
    }

    pub(crate) fn record_region_call(&mut self, retired: usize) {
        increment(&mut self.region_calls, 1);
        increment(&mut self.region_retired, as_u64(retired));
    }

    pub(crate) fn record_region_completed_call(&mut self) {
        increment(&mut self.region_completed_calls, 1);
    }

    pub(crate) fn record_region_guard_exit(&mut self) {
        increment(&mut self.region_guard_exits, 1);
    }

    pub(crate) fn record_region_side_exit(&mut self) {
        increment(&mut self.region_side_exits, 1);
    }

    pub(crate) fn record_region_budget_fallback(&mut self) {
        increment(&mut self.region_budget_fallbacks, 1);
    }

    pub(crate) fn record_loop_call(&mut self, retired: usize, cycle_instructions: usize) {
        increment(&mut self.loop_calls, 1);
        increment(&mut self.loop_retired, as_u64(retired));
        increment(&mut self.loop_cycles, as_u64(retired / cycle_instructions));
    }

    pub(crate) fn record_loop_budget_completion(&mut self) {
        increment(&mut self.loop_budget_completions, 1);
    }

    pub(crate) fn record_loop_guard_exit(&mut self) {
        increment(&mut self.loop_guard_exits, 1);
    }

    pub(crate) fn record_loop_side_exit(&mut self) {
        increment(&mut self.loop_side_exits, 1);
    }

    pub(crate) fn record_loop_budget_fallback(&mut self) {
        increment(&mut self.loop_budget_fallbacks, 1);
    }

    pub(crate) fn record_interpreted_block_call(&mut self) {
        increment(&mut self.interpreted_block_calls, 1);
    }

    pub(crate) fn record_interpreted_attempt(&mut self, instruction: &BlockInstruction) {
        increment(&mut self.interpreted_instructions, 1);
        let Ok(decoded) = instruction else {
            increment(&mut self.interpreted_fetch_traps, 1);
            return;
        };

        let opcode = decoded.opcode() as usize;
        increment(&mut self.interpreted_opcodes[opcode], 1);
        if !rv32vm_rust_x86_block_compiler::supports(*decoded) {
            increment(&mut self.fallback_opcodes[opcode], 1);
        }
    }

    pub(crate) fn record_interpreted_retired(&mut self, retired: u64) {
        increment(&mut self.interpreted_retired, retired);
    }

    pub(crate) fn record_cache_hit(&mut self) {
        increment(&mut self.cache_hits, 1);
    }

    pub(crate) fn record_cache_miss(&mut self) {
        increment(&mut self.cache_misses, 1);
    }

    pub(crate) fn record_transient_translation(&mut self) {
        increment(&mut self.transient_translations, 1);
    }

    pub(crate) fn record_edge_observation(
        &mut self,
        profile_hit: bool,
        replacement: bool,
        dominant: bool,
    ) {
        increment(&mut self.edge_observations, 1);
        increment(&mut self.edge_profile_hits, u64::from(profile_hit));
        increment(&mut self.edge_profile_replacements, u64::from(replacement));
        increment(&mut self.dominant_edge_observations, u64::from(dominant));
    }

    pub(crate) fn record_compile_attempt(&mut self) {
        increment(&mut self.compile_attempts, 1);
    }

    pub(crate) fn record_compiled_code(&mut self, bytes: usize) {
        increment(&mut self.compiled_code_bytes, as_u64(bytes));
    }

    pub(crate) fn record_region_compile_attempt(&mut self) {
        increment(&mut self.region_compile_attempts, 1);
    }

    pub(crate) fn record_region_compiled_code(&mut self, bytes: usize) {
        increment(&mut self.region_compiled_code_bytes, as_u64(bytes));
    }

    pub(crate) fn record_region_path_selected(&mut self, blocks: usize, instructions: usize) {
        increment(&mut self.region_paths_selected, 1);
        increment(&mut self.region_selected_blocks, as_u64(blocks));
        increment(&mut self.region_selected_instructions, as_u64(instructions));
    }

    pub(crate) fn record_region_path_compiled(
        &mut self,
        blocks: usize,
        instructions: usize,
        prefix_fallback: bool,
    ) {
        increment(&mut self.region_compiled_blocks, as_u64(blocks));
        increment(&mut self.region_compiled_instructions, as_u64(instructions));
        increment(
            &mut self.region_path_prefix_fallbacks,
            u64::from(prefix_fallback),
        );
    }

    #[allow(clippy::fn_params_excessive_bools)]
    pub(crate) fn record_region_path_stop(
        &mut self,
        block_limit: bool,
        instruction_limit: bool,
        terminal: bool,
        jalr: bool,
        profile_boundary: bool,
        loop_closure: bool,
    ) {
        increment(
            &mut self.region_path_block_limit_stops,
            u64::from(block_limit),
        );
        increment(
            &mut self.region_path_instruction_limit_stops,
            u64::from(instruction_limit),
        );
        increment(&mut self.region_path_terminal_stops, u64::from(terminal));
        increment(&mut self.region_path_jalr_stops, u64::from(jalr));
        increment(
            &mut self.region_path_profile_stops,
            u64::from(profile_boundary),
        );
        increment(&mut self.region_path_loop_closures, u64::from(loop_closure));
    }

    pub(crate) fn record_loop_compile_attempt(&mut self) {
        increment(&mut self.loop_compile_attempts, 1);
    }

    pub(crate) fn record_loop_compiled_code(&mut self, bytes: usize) {
        increment(&mut self.loop_compiled_code_bytes, as_u64(bytes));
    }

    pub(crate) fn record_region_compile_successes(&mut self, count: usize) {
        increment(&mut self.region_compile_successes, as_u64(count));
    }

    pub(crate) fn record_region_compile_failures(&mut self, count: usize) {
        increment(&mut self.region_compile_failures, as_u64(count));
    }

    pub(crate) fn record_loop_compile_successes(&mut self, count: usize) {
        increment(&mut self.loop_compile_successes, as_u64(count));
    }

    pub(crate) fn record_loop_compile_failures(&mut self, count: usize) {
        increment(&mut self.loop_compile_failures, as_u64(count));
    }

    pub(crate) fn record_compile_successes(
        &mut self,
        count: usize,
        mapped_bytes: usize,
        elapsed: Duration,
    ) {
        increment(&mut self.compile_successes, as_u64(count));
        increment(&mut self.mapped_code_bytes, as_u64(mapped_bytes));
        self.record_compile_elapsed(elapsed);
    }

    pub(crate) fn record_compile_failure(&mut self, reason: CompileFailure, elapsed: Duration) {
        self.record_compile_failures(reason, 1, elapsed);
    }

    pub(crate) fn record_compile_failures(
        &mut self,
        reason: CompileFailure,
        count: usize,
        elapsed: Duration,
    ) {
        let count = as_u64(count);
        increment(&mut self.compile_failures, count);
        match reason {
            CompileFailure::NoCode => increment(&mut self.compile_no_code_failures, count),
            CompileFailure::TooShort => increment(&mut self.compile_too_short_failures, count),
            CompileFailure::Publication => increment(&mut self.compile_publish_failures, count),
        }
        self.record_compile_elapsed(elapsed);
    }

    fn record_compile_elapsed(&mut self, elapsed: Duration) {
        increment(&mut self.compile_elapsed_ns, duration_ns(elapsed));
    }

    fn json(&self) -> String {
        let retired = self.native_retired.saturating_add(self.interpreted_retired);
        let mut output = format!(
            "{{\"schema\":\"rv32vm.vm4.profile\",\"schema_version\":1,\
             \"runs\":{},\"retired\":{retired},\"native_retired\":{},\
             \"interpreted_retired\":{},\"native_calls\":{},\"native_side_exits\":{},\
             \"region_retired\":{},\"region_calls\":{},\"region_completed_calls\":{},\
             \"region_guard_exits\":{},\"region_side_exits\":{},\
             \"region_budget_fallbacks\":{},\
             \"loop_retired\":{},\"loop_calls\":{},\"loop_cycles\":{},\
             \"loop_budget_completions\":{},\"loop_guard_exits\":{},\
             \"loop_side_exits\":{},\"loop_budget_fallbacks\":{},\
             \"interpreted_block_calls\":{},\"interpreted_instructions\":{},\
             \"interpreted_fetch_traps\":{},\"cache_hits\":{},\"cache_misses\":{},\
             \"transient_translations\":{},\"edge_observations\":{},\
             \"edge_profile_hits\":{},\"edge_profile_replacements\":{},\
             \"dominant_edge_observations\":{},\"compile_attempts\":{},\
             \"compile_successes\":{},\"compile_failures\":{},\
             \"compile_no_code_failures\":{},\"compile_too_short_failures\":{},\
             \"compile_publish_failures\":{},\
             \"compiled_code_bytes\":{},\"region_compile_attempts\":{},\
             \"region_compile_successes\":{},\"region_compile_failures\":{},\
             \"region_compiled_code_bytes\":{},\"region_paths_selected\":{},\
             \"region_selected_blocks\":{},\"region_selected_instructions\":{},\
             \"region_compiled_blocks\":{},\"region_compiled_instructions\":{},\
             \"region_path_prefix_fallbacks\":{},\
             \"region_path_block_limit_stops\":{},\
             \"region_path_instruction_limit_stops\":{},\
             \"region_path_terminal_stops\":{},\"region_path_jalr_stops\":{},\
             \"region_path_profile_stops\":{},\"region_path_loop_closures\":{},\
             \"loop_compile_attempts\":{},\
             \"loop_compile_successes\":{},\"loop_compile_failures\":{},\
             \"loop_compiled_code_bytes\":{},\"mapped_code_bytes\":{},\
             \"compile_elapsed_ns\":{},\"run_history_capacity\":{RUN_HISTORY_CAPACITY},\
             \"run_summaries_dropped\":{},\"recent_runs\":[",
            self.runs,
            self.native_retired,
            self.interpreted_retired,
            self.native_calls,
            self.native_side_exits,
            self.region_retired,
            self.region_calls,
            self.region_completed_calls,
            self.region_guard_exits,
            self.region_side_exits,
            self.region_budget_fallbacks,
            self.loop_retired,
            self.loop_calls,
            self.loop_cycles,
            self.loop_budget_completions,
            self.loop_guard_exits,
            self.loop_side_exits,
            self.loop_budget_fallbacks,
            self.interpreted_block_calls,
            self.interpreted_instructions,
            self.interpreted_fetch_traps,
            self.cache_hits,
            self.cache_misses,
            self.transient_translations,
            self.edge_observations,
            self.edge_profile_hits,
            self.edge_profile_replacements,
            self.dominant_edge_observations,
            self.compile_attempts,
            self.compile_successes,
            self.compile_failures,
            self.compile_no_code_failures,
            self.compile_too_short_failures,
            self.compile_publish_failures,
            self.compiled_code_bytes,
            self.region_compile_attempts,
            self.region_compile_successes,
            self.region_compile_failures,
            self.region_compiled_code_bytes,
            self.region_paths_selected,
            self.region_selected_blocks,
            self.region_selected_instructions,
            self.region_compiled_blocks,
            self.region_compiled_instructions,
            self.region_path_prefix_fallbacks,
            self.region_path_block_limit_stops,
            self.region_path_instruction_limit_stops,
            self.region_path_terminal_stops,
            self.region_path_jalr_stops,
            self.region_path_profile_stops,
            self.region_path_loop_closures,
            self.loop_compile_attempts,
            self.loop_compile_successes,
            self.loop_compile_failures,
            self.loop_compiled_code_bytes,
            self.mapped_code_bytes,
            self.compile_elapsed_ns,
            self.runs.saturating_sub(as_u64(self.recent_runs.len())),
        );
        write_run_summaries(&mut output, &self.recent_runs);
        output.push_str("],\"interpreted_opcode_counts\":{");
        write_opcode_counts(&mut output, &self.interpreted_opcodes);
        output.push_str("},\"fallback_opcode_counts\":{");
        write_opcode_counts(&mut output, &self.fallback_opcodes);
        output.push_str("},\"native_side_exit_opcode_counts\":{");
        write_opcode_counts(&mut output, &self.native_side_exit_opcodes);
        output.push_str("}}");
        output
    }

    #[cfg(test)]
    pub(crate) const fn runs(&self) -> u64 {
        self.runs
    }

    #[cfg(test)]
    pub(crate) const fn native_retired(&self) -> u64 {
        self.native_retired
    }

    #[cfg(test)]
    pub(crate) const fn interpreted_retired(&self) -> u64 {
        self.interpreted_retired
    }

    #[cfg(all(
        test,
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    pub(crate) const fn native_side_exits(&self) -> u64 {
        self.native_side_exits
    }

    #[cfg(all(
        test,
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    pub(crate) const fn native_side_exit_opcode_count(&self, opcode: usize) -> u64 {
        self.native_side_exit_opcodes[opcode]
    }

    #[cfg(test)]
    pub(crate) const fn region_calls(&self) -> u64 {
        self.region_calls
    }

    #[cfg(test)]
    pub(crate) const fn region_completed_calls(&self) -> u64 {
        self.region_completed_calls
    }

    #[cfg(test)]
    pub(crate) const fn region_guard_exits(&self) -> u64 {
        self.region_guard_exits
    }

    #[cfg(test)]
    pub(crate) const fn region_side_exits(&self) -> u64 {
        self.region_side_exits
    }

    #[cfg(test)]
    pub(crate) const fn region_budget_fallbacks(&self) -> u64 {
        self.region_budget_fallbacks
    }

    #[cfg(test)]
    pub(crate) const fn loop_retired(&self) -> u64 {
        self.loop_retired
    }

    #[cfg(test)]
    pub(crate) const fn loop_calls(&self) -> u64 {
        self.loop_calls
    }

    #[cfg(test)]
    pub(crate) const fn loop_cycles(&self) -> u64 {
        self.loop_cycles
    }

    #[cfg(test)]
    pub(crate) const fn loop_budget_completions(&self) -> u64 {
        self.loop_budget_completions
    }

    #[cfg(test)]
    pub(crate) const fn loop_guard_exits(&self) -> u64 {
        self.loop_guard_exits
    }

    #[cfg(test)]
    pub(crate) const fn loop_side_exits(&self) -> u64 {
        self.loop_side_exits
    }

    #[cfg(test)]
    pub(crate) const fn loop_budget_fallbacks(&self) -> u64 {
        self.loop_budget_fallbacks
    }

    #[cfg(test)]
    pub(crate) const fn region_compile_attempts(&self) -> u64 {
        self.region_compile_attempts
    }

    #[cfg(test)]
    pub(crate) const fn region_compile_successes(&self) -> u64 {
        self.region_compile_successes
    }

    #[cfg(test)]
    pub(crate) const fn region_compile_failures(&self) -> u64 {
        self.region_compile_failures
    }

    #[cfg(test)]
    pub(crate) const fn region_paths_selected(&self) -> u64 {
        self.region_paths_selected
    }

    #[cfg(test)]
    pub(crate) const fn region_selected_blocks(&self) -> u64 {
        self.region_selected_blocks
    }

    #[cfg(test)]
    pub(crate) const fn region_selected_instructions(&self) -> u64 {
        self.region_selected_instructions
    }

    #[cfg(test)]
    pub(crate) const fn region_compiled_blocks(&self) -> u64 {
        self.region_compiled_blocks
    }

    #[cfg(test)]
    pub(crate) const fn region_compiled_instructions(&self) -> u64 {
        self.region_compiled_instructions
    }

    #[cfg(test)]
    pub(crate) const fn region_path_prefix_fallbacks(&self) -> u64 {
        self.region_path_prefix_fallbacks
    }

    #[cfg(test)]
    pub(crate) const fn region_path_block_limit_stops(&self) -> u64 {
        self.region_path_block_limit_stops
    }

    #[cfg(test)]
    pub(crate) const fn region_path_instruction_limit_stops(&self) -> u64 {
        self.region_path_instruction_limit_stops
    }

    #[cfg(test)]
    pub(crate) const fn region_path_terminal_stops(&self) -> u64 {
        self.region_path_terminal_stops
    }

    #[cfg(test)]
    pub(crate) const fn region_path_jalr_stops(&self) -> u64 {
        self.region_path_jalr_stops
    }

    #[cfg(test)]
    pub(crate) const fn region_path_profile_stops(&self) -> u64 {
        self.region_path_profile_stops
    }

    #[cfg(test)]
    pub(crate) const fn region_path_loop_closures(&self) -> u64 {
        self.region_path_loop_closures
    }

    #[cfg(test)]
    pub(crate) const fn loop_compile_attempts(&self) -> u64 {
        self.loop_compile_attempts
    }

    #[cfg(test)]
    pub(crate) const fn loop_compile_successes(&self) -> u64 {
        self.loop_compile_successes
    }

    #[cfg(test)]
    pub(crate) const fn loop_compile_failures(&self) -> u64 {
        self.loop_compile_failures
    }

    #[cfg(test)]
    pub(crate) const fn interpreted_block_calls(&self) -> u64 {
        self.interpreted_block_calls
    }

    #[cfg(test)]
    pub(crate) const fn cache_hits(&self) -> u64 {
        self.cache_hits
    }

    #[cfg(test)]
    pub(crate) const fn cache_misses(&self) -> u64 {
        self.cache_misses
    }

    #[cfg(test)]
    pub(crate) const fn edge_observations(&self) -> u64 {
        self.edge_observations
    }

    #[cfg(test)]
    pub(crate) const fn edge_profile_hits(&self) -> u64 {
        self.edge_profile_hits
    }

    #[cfg(test)]
    pub(crate) const fn edge_profile_replacements(&self) -> u64 {
        self.edge_profile_replacements
    }

    #[cfg(test)]
    pub(crate) fn recent_run_count(&self) -> usize {
        self.recent_runs.len()
    }

    #[cfg(test)]
    pub(crate) fn most_recent_run_retired(&self) -> Option<u64> {
        self.recent_runs.back().map(|summary| {
            summary
                .metrics
                .native_retired
                .saturating_add(summary.metrics.interpreted_retired)
        })
    }
}

fn increment(counter: &mut u64, amount: u64) {
    *counter = counter.saturating_add(amount);
}

fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn duration_ns(value: Duration) -> u64 {
    u64::try_from(value.as_nanos()).unwrap_or(u64::MAX)
}

fn write_run_summaries(output: &mut String, summaries: &VecDeque<RunSummary>) {
    for (index, summary) in summaries.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        let metrics = summary.metrics;
        let retired = metrics
            .native_retired
            .saturating_add(metrics.interpreted_retired);
        write!(
            output,
            "{{\"run\":{},\"retired\":{retired},\"native_retired\":{},\
             \"interpreted_retired\":{},\"native_calls\":{},\"native_side_exits\":{},\
             \"region_retired\":{},\"region_calls\":{},\"region_completed_calls\":{},\
             \"region_guard_exits\":{},\"region_side_exits\":{},\
             \"region_budget_fallbacks\":{},\
             \"loop_retired\":{},\"loop_calls\":{},\"loop_cycles\":{},\
             \"loop_budget_completions\":{},\"loop_guard_exits\":{},\
             \"loop_side_exits\":{},\"loop_budget_fallbacks\":{},\
             \"interpreted_block_calls\":{},\"interpreted_instructions\":{},\
             \"interpreted_fetch_traps\":{},\"cache_hits\":{},\"cache_misses\":{},\
             \"transient_translations\":{},\"edge_observations\":{},\
             \"edge_profile_hits\":{},\"edge_profile_replacements\":{},\
             \"dominant_edge_observations\":{},\"compile_attempts\":{},\
             \"compile_successes\":{},\"compile_failures\":{},\
             \"compile_no_code_failures\":{},\"compile_too_short_failures\":{},\
             \"compile_publish_failures\":{},\
             \"compiled_code_bytes\":{},\"region_compile_attempts\":{},\
             \"region_compile_successes\":{},\"region_compile_failures\":{},\
             \"region_compiled_code_bytes\":{},\"region_paths_selected\":{},\
             \"region_selected_blocks\":{},\"region_selected_instructions\":{},\
             \"region_compiled_blocks\":{},\"region_compiled_instructions\":{},\
             \"region_path_prefix_fallbacks\":{},\
             \"region_path_block_limit_stops\":{},\
             \"region_path_instruction_limit_stops\":{},\
             \"region_path_terminal_stops\":{},\"region_path_jalr_stops\":{},\
             \"region_path_profile_stops\":{},\"region_path_loop_closures\":{},\
             \"loop_compile_attempts\":{},\
             \"loop_compile_successes\":{},\"loop_compile_failures\":{},\
             \"loop_compiled_code_bytes\":{},\"mapped_code_bytes\":{},\
             \"compile_elapsed_ns\":{}}}",
            summary.run,
            metrics.native_retired,
            metrics.interpreted_retired,
            metrics.native_calls,
            metrics.native_side_exits,
            metrics.region_retired,
            metrics.region_calls,
            metrics.region_completed_calls,
            metrics.region_guard_exits,
            metrics.region_side_exits,
            metrics.region_budget_fallbacks,
            metrics.loop_retired,
            metrics.loop_calls,
            metrics.loop_cycles,
            metrics.loop_budget_completions,
            metrics.loop_guard_exits,
            metrics.loop_side_exits,
            metrics.loop_budget_fallbacks,
            metrics.interpreted_block_calls,
            metrics.interpreted_instructions,
            metrics.interpreted_fetch_traps,
            metrics.cache_hits,
            metrics.cache_misses,
            metrics.transient_translations,
            metrics.edge_observations,
            metrics.edge_profile_hits,
            metrics.edge_profile_replacements,
            metrics.dominant_edge_observations,
            metrics.compile_attempts,
            metrics.compile_successes,
            metrics.compile_failures,
            metrics.compile_no_code_failures,
            metrics.compile_too_short_failures,
            metrics.compile_publish_failures,
            metrics.compiled_code_bytes,
            metrics.region_compile_attempts,
            metrics.region_compile_successes,
            metrics.region_compile_failures,
            metrics.region_compiled_code_bytes,
            metrics.region_paths_selected,
            metrics.region_selected_blocks,
            metrics.region_selected_instructions,
            metrics.region_compiled_blocks,
            metrics.region_compiled_instructions,
            metrics.region_path_prefix_fallbacks,
            metrics.region_path_block_limit_stops,
            metrics.region_path_instruction_limit_stops,
            metrics.region_path_terminal_stops,
            metrics.region_path_jalr_stops,
            metrics.region_path_profile_stops,
            metrics.region_path_loop_closures,
            metrics.loop_compile_attempts,
            metrics.loop_compile_successes,
            metrics.loop_compile_failures,
            metrics.loop_compiled_code_bytes,
            metrics.mapped_code_bytes,
            metrics.compile_elapsed_ns,
        )
        .expect("writing to a string cannot fail");
    }
}

fn write_opcode_counts(output: &mut String, counts: &[u64; OPCODE_COUNT]) {
    let mut first = true;
    for (opcode, count) in counts.iter().enumerate().filter(|(_, count)| **count != 0) {
        if !first {
            output.push(',');
        }
        first = false;
        write!(output, "\"0x{opcode:02x}\":{count}").expect("writing to a string cannot fail");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rv32vm_rust_common::memory::IMAGE_START;

    use super::{CompileFailure, ProfileCounters, RUN_HISTORY_CAPACITY};
    use crate::test_support::{addi, machine_with_code_at};

    #[test]
    fn renders_stable_machine_readable_counters_and_sparse_opcode_maps() {
        let machine = machine_with_code_at(&[addi(5, 0, 1), 0x0000_0073], IMAGE_START);
        let mut profile = ProfileCounters::default();
        profile.start_image();
        profile.begin_run();
        profile.record_native_call(0);
        profile.record_native_side_exit(&machine.fetch_decode(IMAGE_START));
        profile.record_interpreted_block_call();
        profile.record_interpreted_attempt(&machine.fetch_decode(IMAGE_START));
        profile.record_interpreted_retired(1);
        profile.record_interpreted_attempt(&machine.fetch_decode(IMAGE_START + 4));
        profile.record_cache_miss();
        profile.record_edge_observation(false, false, false);
        profile.record_edge_observation(true, false, true);
        profile.record_edge_observation(false, true, false);
        profile.record_compile_attempt();
        profile.record_compiled_code(17);
        profile.record_compile_failure(CompileFailure::TooShort, Duration::from_nanos(23));
        profile.end_run();

        assert_eq!(
            profile.json(),
            "{\"schema\":\"rv32vm.vm4.profile\",\"schema_version\":1,\"runs\":1,\
             \"retired\":1,\"native_retired\":0,\"interpreted_retired\":1,\
             \"native_calls\":1,\"native_side_exits\":1,\"region_retired\":0,\
             \"region_calls\":0,\"region_completed_calls\":0,\
             \"region_guard_exits\":0,\"region_side_exits\":0,\
             \"region_budget_fallbacks\":0,\"loop_retired\":0,\"loop_calls\":0,\
             \"loop_cycles\":0,\"loop_budget_completions\":0,\
             \"loop_guard_exits\":0,\"loop_side_exits\":0,\
             \"loop_budget_fallbacks\":0,\
             \"interpreted_block_calls\":1,\
             \"interpreted_instructions\":2,\"interpreted_fetch_traps\":0,\
             \"cache_hits\":0,\"cache_misses\":1,\"transient_translations\":0,\
             \"edge_observations\":3,\"edge_profile_hits\":1,\
             \"edge_profile_replacements\":1,\"dominant_edge_observations\":1,\
             \"compile_attempts\":1,\"compile_successes\":0,\"compile_failures\":1,\
             \"compile_no_code_failures\":0,\"compile_too_short_failures\":1,\
             \"compile_publish_failures\":0,\
             \"compiled_code_bytes\":17,\"region_compile_attempts\":0,\
             \"region_compile_successes\":0,\"region_compile_failures\":0,\
             \"region_compiled_code_bytes\":0,\"region_paths_selected\":0,\
             \"region_selected_blocks\":0,\"region_selected_instructions\":0,\
             \"region_compiled_blocks\":0,\"region_compiled_instructions\":0,\
             \"region_path_prefix_fallbacks\":0,\
             \"region_path_block_limit_stops\":0,\
             \"region_path_instruction_limit_stops\":0,\
             \"region_path_terminal_stops\":0,\"region_path_jalr_stops\":0,\
             \"region_path_profile_stops\":0,\"region_path_loop_closures\":0,\
             \"loop_compile_attempts\":0,\
             \"loop_compile_successes\":0,\"loop_compile_failures\":0,\
             \"loop_compiled_code_bytes\":0,\"mapped_code_bytes\":0,\
             \"compile_elapsed_ns\":23,\
             \"run_history_capacity\":64,\"run_summaries_dropped\":0,\"recent_runs\":[{\
             \"run\":1,\"retired\":1,\"native_retired\":0,\"interpreted_retired\":1,\
             \"native_calls\":1,\"native_side_exits\":1,\"region_retired\":0,\
             \"region_calls\":0,\"region_completed_calls\":0,\
             \"region_guard_exits\":0,\"region_side_exits\":0,\
             \"region_budget_fallbacks\":0,\"loop_retired\":0,\"loop_calls\":0,\
             \"loop_cycles\":0,\"loop_budget_completions\":0,\
             \"loop_guard_exits\":0,\"loop_side_exits\":0,\
             \"loop_budget_fallbacks\":0,\
             \"interpreted_block_calls\":1,\
             \"interpreted_instructions\":2,\
             \"interpreted_fetch_traps\":0,\"cache_hits\":0,\"cache_misses\":1,\
             \"transient_translations\":0,\"edge_observations\":3,\
             \"edge_profile_hits\":1,\"edge_profile_replacements\":1,\
             \"dominant_edge_observations\":1,\"compile_attempts\":1,\
             \"compile_successes\":0,\
             \"compile_failures\":1,\"compile_no_code_failures\":0,\
             \"compile_too_short_failures\":1,\"compile_publish_failures\":0,\
             \"compiled_code_bytes\":17,\"region_compile_attempts\":0,\
             \"region_compile_successes\":0,\"region_compile_failures\":0,\
             \"region_compiled_code_bytes\":0,\"region_paths_selected\":0,\
             \"region_selected_blocks\":0,\"region_selected_instructions\":0,\
             \"region_compiled_blocks\":0,\"region_compiled_instructions\":0,\
             \"region_path_prefix_fallbacks\":0,\
             \"region_path_block_limit_stops\":0,\
             \"region_path_instruction_limit_stops\":0,\
             \"region_path_terminal_stops\":0,\"region_path_jalr_stops\":0,\
             \"region_path_profile_stops\":0,\"region_path_loop_closures\":0,\
             \"loop_compile_attempts\":0,\
             \"loop_compile_successes\":0,\"loop_compile_failures\":0,\
             \"loop_compiled_code_bytes\":0,\"mapped_code_bytes\":0,\
             \"compile_elapsed_ns\":23}],\
             \"interpreted_opcode_counts\":{\"0x13\":1,\"0x73\":1},\
             \"fallback_opcode_counts\":{\"0x73\":1},\
             \"native_side_exit_opcode_counts\":{\"0x13\":1}}"
        );
        assert_eq!(profile.region_calls(), 0);
        assert_eq!(profile.region_completed_calls(), 0);
        assert_eq!(profile.region_guard_exits(), 0);
        assert_eq!(profile.region_side_exits(), 0);
        assert_eq!(profile.region_budget_fallbacks(), 0);
        assert_eq!(profile.loop_retired(), 0);
        assert_eq!(profile.loop_calls(), 0);
        assert_eq!(profile.loop_cycles(), 0);
        assert_eq!(profile.loop_budget_completions(), 0);
        assert_eq!(profile.loop_guard_exits(), 0);
        assert_eq!(profile.loop_side_exits(), 0);
        assert_eq!(profile.loop_budget_fallbacks(), 0);
        assert_eq!(profile.region_compile_attempts(), 0);
        assert_eq!(profile.region_compile_successes(), 0);
        assert_eq!(profile.region_compile_failures(), 0);
        assert_eq!(profile.loop_compile_attempts(), 0);
        assert_eq!(profile.loop_compile_successes(), 0);
        assert_eq!(profile.loop_compile_failures(), 0);
    }

    #[test]
    fn retains_only_the_most_recent_run_summaries() {
        let mut profile = ProfileCounters::default();
        profile.start_image();

        for _ in 0..RUN_HISTORY_CAPACITY + 1 {
            profile.begin_run();
            profile.record_interpreted_retired(1);
            profile.end_run();
        }

        assert_eq!(profile.recent_runs.len(), RUN_HISTORY_CAPACITY);
        assert_eq!(profile.recent_runs.front().unwrap().run, 2);
        assert_eq!(
            profile.recent_runs.back().unwrap().run,
            RUN_HISTORY_CAPACITY as u64 + 1
        );
    }

    #[test]
    fn serializes_region_path_depth_and_stop_counters() {
        let mut profile = ProfileCounters::default();
        profile.start_image();
        profile.begin_run();
        profile.record_region_path_selected(8, 240);
        profile.record_region_path_compiled(7, 192, true);
        profile.record_region_path_stop(true, true, true, true, true, true);
        profile.end_run();

        assert_eq!(profile.region_paths_selected(), 1);
        assert_eq!(profile.region_selected_blocks(), 8);
        assert_eq!(profile.region_selected_instructions(), 240);
        assert_eq!(profile.region_compiled_blocks(), 7);
        assert_eq!(profile.region_compiled_instructions(), 192);
        assert_eq!(profile.region_path_prefix_fallbacks(), 1);
        assert_eq!(profile.region_path_block_limit_stops(), 1);
        assert_eq!(profile.region_path_instruction_limit_stops(), 1);
        assert_eq!(profile.region_path_terminal_stops(), 1);
        assert_eq!(profile.region_path_jalr_stops(), 1);
        assert_eq!(profile.region_path_profile_stops(), 1);
        assert_eq!(profile.region_path_loop_closures(), 1);

        let json = profile.json();
        for field in [
            "\"region_paths_selected\":1",
            "\"region_selected_blocks\":8",
            "\"region_selected_instructions\":240",
            "\"region_compiled_blocks\":7",
            "\"region_compiled_instructions\":192",
            "\"region_path_prefix_fallbacks\":1",
            "\"region_path_block_limit_stops\":1",
            "\"region_path_instruction_limit_stops\":1",
            "\"region_path_terminal_stops\":1",
            "\"region_path_jalr_stops\":1",
            "\"region_path_profile_stops\":1",
            "\"region_path_loop_closures\":1",
        ] {
            assert_eq!(json.matches(field).count(), 2, "missing {field}");
        }
    }

    #[test]
    fn serializes_nonzero_loop_subcounters_in_aggregate_and_recent_metrics() {
        let mut profile = ProfileCounters::default();
        profile.start_image();
        profile.begin_run();
        profile.record_loop_call(11, 3);
        profile.record_loop_budget_completion();
        profile.record_loop_guard_exit();
        profile.record_loop_side_exit();
        profile.record_loop_budget_fallback();
        profile.record_loop_compile_attempt();
        profile.record_loop_compile_attempt();
        profile.record_loop_compiled_code(17);
        profile.record_loop_compile_successes(1);
        profile.record_loop_compile_failures(1);
        profile.end_run();

        let recent = &profile.recent_runs.front().unwrap().metrics;
        assert_eq!(recent.loop_retired, 11);
        assert_eq!(recent.loop_calls, 1);
        assert_eq!(recent.loop_cycles, 3);
        assert_eq!(recent.loop_budget_completions, 1);
        assert_eq!(recent.loop_guard_exits, 1);
        assert_eq!(recent.loop_side_exits, 1);
        assert_eq!(recent.loop_budget_fallbacks, 1);
        assert_eq!(recent.loop_compile_attempts, 2);
        assert_eq!(recent.loop_compile_successes, 1);
        assert_eq!(recent.loop_compile_failures, 1);
        assert_eq!(recent.loop_compiled_code_bytes, 17);

        let json = profile.json();
        for field in [
            "\"loop_retired\":11",
            "\"loop_calls\":1",
            "\"loop_cycles\":3",
            "\"loop_budget_completions\":1",
            "\"loop_guard_exits\":1",
            "\"loop_side_exits\":1",
            "\"loop_budget_fallbacks\":1",
            "\"loop_compile_attempts\":2",
            "\"loop_compile_successes\":1",
            "\"loop_compile_failures\":1",
            "\"loop_compiled_code_bytes\":17",
        ] {
            assert_eq!(json.matches(field).count(), 2, "missing {field}");
        }
    }
}
