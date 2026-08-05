//! Owns image-scoped blocks and their tiering state.

use std::num::NonZeroU32;
#[cfg(feature = "profile")]
use std::time::{Duration, Instant};

use rv32vm_rust_common::{
    machine::Machine,
    memory::{PAGE_COUNT, PAGE_SHIFT, PAGE_SIZE},
};
use rv32vm_rust_x86_block_compiler::{
    CompiledBlock, NativeEntry, NativeEntryKind, NativeProgram, RegionBlock, RegionLimits,
};

use crate::block::BasicBlock;
#[cfg(feature = "profile")]
use crate::profile::{CompileFailure, ProfileCounters};

const INSTRUCTIONS_PER_PAGE: usize = PAGE_SIZE / 4;
const MAX_BLOCKS: usize = 8_192;
const MAX_DECODED_INSTRUCTIONS: usize = 262_144;
const COMPILATION_THRESHOLD: u8 = 3;
/// Hot single-instruction control-flow blocks are common enough to publish.
const MIN_NATIVE_INSTRUCTIONS: usize = 1;
/// Keep each demand-published cohort near one executable page.
const COHORT_CODE_BYTES: usize = PAGE_SIZE;
/// Bound staging metadata even when generated blocks are unusually small.
const MAX_COHORT_ENTRIES: usize = 32;
/// Publish a partial cohort after this many dispatches revisit staged blocks.
pub(crate) const STAGED_REVISIT_FLUSH_INTERVAL: usize = 1_024;
const EDGE_PROFILE_WIDTH: usize = 2;
const DOMINANT_EDGE_MINIMUM_OBSERVATIONS: u32 = 8;
const DOMINANT_PATH_MINIMUM_OBSERVATIONS: u32 = DOMINANT_EDGE_MINIMUM_OBSERVATIONS - 1;
const DOMINANT_EDGE_NUMERATOR: u32 = 7;
const DOMINANT_EDGE_DENOMINATOR: u32 = 8;
const MAX_REGION_EDGE_OBSERVATIONS: u32 = 64;
const MAX_BOUNDED_REGION_CODE_BYTES: usize = 64 * 1_024;
pub(crate) const VM4_REGION_LIMITS: RegionLimits =
    RegionLimits::new(8, 256, MAX_BOUNDED_REGION_CODE_BYTES);
const VM4_REGION_MAX_BLOCKS: usize = VM4_REGION_LIMITS.max_blocks();
const VM4_REGION_MAX_INSTRUCTIONS: usize = VM4_REGION_LIMITS.max_instructions();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlockId(NonZeroU32);

impl BlockId {
    fn new(index: usize) -> Self {
        let value = u32::try_from(index + 1).expect("cache limit fits in u32");
        Self(NonZeroU32::new(value).expect("block IDs are one-based"))
    }

    const fn index(self) -> usize {
        (self.0.get() - 1) as usize
    }
}

struct BlockPage {
    entries: [Option<BlockId>; INSTRUCTIONS_PER_PAGE],
}

impl BlockPage {
    fn new() -> Self {
        Self {
            entries: [None; INSTRUCTIONS_PER_PAGE],
        }
    }
}

enum BasicTier {
    Profiling(u8),
    Staged,
    Native(NativeHandle),
    Disabled,
}

enum RegionTier {
    Profiling,
    Staged,
    Native(RegionHandle),
    Disabled,
}

#[derive(Clone, Copy)]
struct NativeHandle {
    program: usize,
    entry: usize,
}

#[derive(Clone, Copy)]
struct RegionBoundary {
    retired: u32,
    source: BlockId,
}

#[derive(Clone, Copy)]
pub(crate) struct RegionMetadata {
    // Compact u32 retirement boundaries keep eight-entry metadata the same
    // size as the former four-entry usize representation on x86-64.
    boundaries: [RegionBoundary; VM4_REGION_MAX_BLOCKS],
    block_count: usize,
    shape: RegionShape,
}

#[derive(Clone, Copy)]
enum RegionShape {
    Bounded,
    Loop {
        head_pc: u32,
        cycle_instructions: usize,
    },
}

impl RegionMetadata {
    fn new(
        path: &[BlockId],
        blocks: &[CachedBlock],
        instruction_count: usize,
        shape: RegionShape,
    ) -> Self {
        let final_source = *path.last().expect("compiled regions have a final block");
        debug_assert!(path.len() <= VM4_REGION_MAX_BLOCKS);
        let final_retired =
            u32::try_from(instruction_count).expect("VM4 region instruction limits fit in u32");
        let mut boundaries = [RegionBoundary {
            retired: final_retired,
            source: final_source,
        }; VM4_REGION_MAX_BLOCKS];
        let mut retired = 0_usize;
        for (index, &source) in path.iter().enumerate() {
            if index + 1 == path.len() {
                boundaries[index] = RegionBoundary {
                    retired: final_retired,
                    source,
                };
            } else {
                retired = retired
                    .checked_add(blocks[source.index()].block.len())
                    .expect("bounded region retirement cannot overflow");
                debug_assert!(retired < instruction_count);
                boundaries[index] = RegionBoundary {
                    retired: u32::try_from(retired)
                        .expect("VM4 region instruction limits fit in u32"),
                    source,
                };
            }
        }
        Self {
            boundaries,
            block_count: path.len(),
            shape,
        }
    }

    pub(crate) fn source_for_retired(&self, retired: usize) -> Option<BlockId> {
        let retired = match self.shape {
            RegionShape::Bounded => retired,
            RegionShape::Loop {
                cycle_instructions, ..
            } => {
                let residue = retired % cycle_instructions;
                if residue == 0 {
                    return Some(self.final_source());
                }
                residue
            }
        };
        self.boundaries[..self.block_count]
            .iter()
            .find(|boundary| boundary.retired as usize == retired)
            .map(|boundary| boundary.source)
    }

    pub(crate) fn final_source(&self) -> BlockId {
        self.boundaries[self.block_count - 1].source
    }

    pub(crate) const fn is_loop(&self) -> bool {
        matches!(self.shape, RegionShape::Loop { .. })
    }

    #[cfg(feature = "profile")]
    pub(crate) const fn cycle_instructions(&self) -> Option<usize> {
        match self.shape {
            RegionShape::Bounded => None,
            RegionShape::Loop {
                cycle_instructions, ..
            } => Some(cycle_instructions),
        }
    }

    pub(crate) fn is_loop_budget_completion(&self, retired: usize, next_pc: u32) -> bool {
        match self.shape {
            RegionShape::Bounded => false,
            RegionShape::Loop {
                head_pc,
                cycle_instructions,
            } => retired != 0 && retired.is_multiple_of(cycle_instructions) && next_pc == head_pc,
        }
    }

    #[cfg(all(
        test,
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    pub(crate) const fn block_count(&self) -> usize {
        self.block_count
    }
}

#[derive(Clone, Copy)]
struct RegionHandle {
    native: NativeHandle,
    metadata: RegionMetadata,
}

#[derive(Clone, Copy)]
pub(crate) struct NativeRegionEntry<'a> {
    pub(crate) entry: NativeEntry<'a>,
    pub(crate) metadata: &'a RegionMetadata,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SuccessorProfile {
    pub(crate) target_pc: u32,
    pub(crate) target: BlockId,
    pub(crate) observations: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EdgeSnapshot {
    pub(crate) successors: [Option<SuccessorProfile>; EDGE_PROFILE_WIDTH],
    pub(crate) observations: u32,
}

impl EdgeSnapshot {
    pub(crate) fn dominant_successor(
        self,
        minimum_observations: u32,
        numerator: u32,
        denominator: u32,
    ) -> Option<SuccessorProfile> {
        if denominator == 0 || numerator > denominator || self.observations < minimum_observations {
            return None;
        }
        let successor = self
            .successors
            .into_iter()
            .flatten()
            .max_by_key(|successor| successor.observations)?;
        (u64::from(successor.observations) * u64::from(denominator)
            >= u64::from(self.observations) * u64::from(numerator))
        .then_some(successor)
    }
}

#[derive(Clone, Copy, Default)]
struct EdgeProfile {
    successors: [Option<SuccessorProfile>; EDGE_PROFILE_WIDTH],
    observations: u32,
}

enum EdgeUpdate {
    Inserted,
    Hit,
    Replaced,
}

impl EdgeProfile {
    fn observe(&mut self, target_pc: u32, target: BlockId) -> EdgeUpdate {
        self.observations = self.observations.saturating_add(1);
        if let Some(successor) = self
            .successors
            .iter_mut()
            .flatten()
            .find(|successor| successor.target_pc == target_pc)
        {
            successor.target = target;
            successor.observations = successor.observations.saturating_add(1);
            return EdgeUpdate::Hit;
        }

        let successor = SuccessorProfile {
            target_pc,
            target,
            observations: 1,
        };
        if let Some(empty) = self.successors.iter_mut().find(|slot| slot.is_none()) {
            *empty = Some(successor);
            return EdgeUpdate::Inserted;
        }

        let first = self.successors[0].expect("full edge profile has a first successor");
        let second = self.successors[1].expect("full edge profile has a second successor");
        let replacement = usize::from(second.observations < first.observations);
        self.successors[replacement] = Some(successor);
        EdgeUpdate::Replaced
    }

    const fn snapshot(self) -> EdgeSnapshot {
        EdgeSnapshot {
            successors: self.successors,
            observations: self.observations,
        }
    }
}

/// A decoded block together with its profiling or native state.
pub(crate) struct CachedBlock {
    start_pc: u32,
    block: BasicBlock,
    basic_tier: BasicTier,
    region_tier: RegionTier,
    edges: EdgeProfile,
}

impl CachedBlock {
    fn new(start_pc: u32, block: BasicBlock) -> Self {
        Self {
            start_pc,
            block,
            basic_tier: BasicTier::Profiling(0),
            region_tier: RegionTier::Profiling,
            edges: EdgeProfile::default(),
        }
    }
}

#[derive(Clone, Copy)]
enum StagedOwner {
    Basic(BlockId),
    Region {
        head: BlockId,
        metadata: RegionMetadata,
    },
}

enum RegionPathSelection {
    Pending,
    Terminal(RegionPathStop),
    Ready(DominantRegionPath),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegionPathStop {
    BlockLimit,
    InstructionLimit,
    Terminal,
    Jalr,
    ProfileBoundary,
    LoopClosure,
}

struct DominantRegionPath {
    blocks: Vec<BlockId>,
    head_closing_prefix: Option<usize>,
    #[cfg_attr(not(feature = "profile"), allow(dead_code))]
    instruction_count: usize,
    #[cfg_attr(not(feature = "profile"), allow(dead_code))]
    stop: RegionPathStop,
}

fn stopped_region_path(
    blocks: Vec<BlockId>,
    head_closing_prefix: Option<usize>,
    instruction_count: usize,
    stop: RegionPathStop,
) -> RegionPathSelection {
    if blocks.len() < 2 && head_closing_prefix.is_none() {
        RegionPathSelection::Terminal(stop)
    } else {
        RegionPathSelection::Ready(DominantRegionPath {
            blocks,
            head_closing_prefix,
            instruction_count,
            stop,
        })
    }
}

fn block_ends_jalr(block: &CachedBlock) -> bool {
    block
        .block
        .instructions()
        .last()
        .is_some_and(|instruction| {
            instruction
                .as_ref()
                .is_ok_and(|decoded| decoded.opcode() == 0x67 && decoded.funct3() == 0)
        })
}

struct StagedEntry {
    owner: StagedOwner,
    compiled: CompiledBlock,
    #[cfg(feature = "profile")]
    compile_elapsed: Duration,
}

pub(crate) enum BlockLookup {
    Cached(BlockId),
    Transient(BasicBlock),
}

/// Exact native successor selected from immutable edge metadata.
#[derive(Clone, Copy)]
pub(crate) enum NativeContinuation<'a> {
    /// The source must return through the ordinary dispatcher so its edge can
    /// still contribute to demand-driven region formation.
    Profiling,
    /// The actual successor is not present in the source's bounded profile.
    Miss,
    /// The exact target needs publication, compilation, or interpretation.
    Unavailable,
    /// Native code exists, but the exact remaining budget cannot enter it.
    Budget,
    /// The current basic tier is ready. The flag preserves a region budget
    /// fallback that ordinary dispatch would have profiled first.
    Basic {
        entry: NativeEntry<'a>,
        source: BlockId,
        region_budget_fallback: bool,
        region_loop_budget_fallback: bool,
    },
    /// The current region tier is ready and remains preferred over basic code.
    Region(NativeRegionEntry<'a>),
}

/// A bounded sparse cache indexed by guest program counter.
pub(crate) struct BlockCache {
    pages: Box<[Option<Box<BlockPage>>]>,
    blocks: Vec<CachedBlock>,
    programs: Vec<NativeProgram>,
    staged: Vec<StagedEntry>,
    staged_code_bytes: usize,
    staged_revisits: usize,
    decoded_instructions: usize,
    maximum_blocks: usize,
    maximum_decoded_instructions: usize,
}

impl Default for BlockCache {
    fn default() -> Self {
        Self {
            pages: std::iter::repeat_with(|| None).take(PAGE_COUNT).collect(),
            blocks: Vec::new(),
            programs: Vec::new(),
            staged: Vec::new(),
            staged_code_bytes: 0,
            staged_revisits: 0,
            decoded_instructions: 0,
            maximum_blocks: MAX_BLOCKS,
            maximum_decoded_instructions: MAX_DECODED_INSTRUCTIONS,
        }
    }
}

impl BlockCache {
    #[cfg(test)]
    pub(crate) fn with_limits(maximum_blocks: usize, maximum_decoded_instructions: usize) -> Self {
        Self {
            maximum_blocks,
            maximum_decoded_instructions,
            ..Self::default()
        }
    }

    pub(crate) fn clear(&mut self) {
        for page in &mut self.pages {
            *page = None;
        }
        self.staged.clear();
        self.staged_code_bytes = 0;
        self.staged_revisits = 0;
        self.programs.clear();
        self.blocks.clear();
        self.decoded_instructions = 0;
    }

    pub(crate) fn block(&self, id: BlockId) -> &BasicBlock {
        &self.blocks[id.index()].block
    }

    pub(crate) fn native_entry(&self, id: BlockId) -> Option<NativeEntry<'_>> {
        let BasicTier::Native(handle) = self.blocks[id.index()].basic_tier else {
            return None;
        };
        self.programs.get(handle.program)?.entry(handle.entry)
    }

    pub(crate) fn native_region_entry(&self, id: BlockId) -> Option<NativeRegionEntry<'_>> {
        let RegionTier::Native(handle) = &self.blocks[id.index()].region_tier else {
            return None;
        };
        Some(NativeRegionEntry {
            entry: self
                .programs
                .get(handle.native.program)?
                .entry(handle.native.entry)?,
            metadata: &handle.metadata,
        })
    }

    pub(crate) fn observe_edge(
        &mut self,
        source: BlockId,
        target_pc: u32,
        target: BlockId,
        #[cfg(feature = "profile")] profile: &mut ProfileCounters,
    ) -> bool {
        let Some(source_block) = self.blocks.get(source.index()) else {
            return false;
        };
        if !matches!(source_block.region_tier, RegionTier::Profiling) {
            return false;
        }
        let Some(target_block) = self.blocks.get(target.index()) else {
            return false;
        };
        if target_block.start_pc != target_pc {
            return false;
        }

        let update = self.blocks[source.index()].edges.observe(target_pc, target);
        let snapshot = self.blocks[source.index()].edges.snapshot();
        let dominant = snapshot.dominant_successor(
            DOMINANT_EDGE_MINIMUM_OBSERVATIONS,
            DOMINANT_EDGE_NUMERATOR,
            DOMINANT_EDGE_DENOMINATOR,
        );
        if snapshot.observations >= MAX_REGION_EDGE_OBSERVATIONS && dominant.is_none() {
            self.blocks[source.index()].region_tier = RegionTier::Disabled;
        }
        #[cfg(not(feature = "profile"))]
        let _ = update;
        #[cfg(feature = "profile")]
        profile.record_edge_observation(
            matches!(update, EdgeUpdate::Hit),
            matches!(update, EdgeUpdate::Replaced),
            dominant.is_some(),
        );
        true
    }

    pub(crate) fn profiles_edges(&self, id: BlockId) -> bool {
        self.blocks
            .get(id.index())
            .is_some_and(|block| matches!(block.region_tier, RegionTier::Profiling))
    }

    /// Resolves an exact successor only after the source edge profile freezes.
    ///
    /// `observe_edge` mutates metadata exclusively while `region_tier` is
    /// `Profiling`, so every other tier makes these bounded entries immutable.
    /// Matching both the recorded PC and the target block's current start PC
    /// prevents a stale or inconsistent ID from becoming a continuation.
    pub(crate) fn native_continuation(
        &self,
        source: BlockId,
        target_pc: u32,
        remaining: u64,
    ) -> NativeContinuation<'_> {
        let Some(source) = self.blocks.get(source.index()) else {
            return NativeContinuation::Miss;
        };
        if matches!(source.region_tier, RegionTier::Profiling) {
            return NativeContinuation::Profiling;
        }
        let Some(successor) = source
            .edges
            .successors
            .iter()
            .flatten()
            .find(|successor| successor.target_pc == target_pc)
        else {
            return NativeContinuation::Miss;
        };
        let Some(block) = self
            .blocks
            .get(successor.target.index())
            .filter(|target| target.start_pc == target_pc)
        else {
            return NativeContinuation::Miss;
        };
        if matches!(block.region_tier, RegionTier::Staged)
            || matches!(block.basic_tier, BasicTier::Staged)
        {
            return NativeContinuation::Unavailable;
        }

        let mut region_budget_fallback = false;
        let mut region_loop_budget_fallback = false;
        if let RegionTier::Native(handle) = &block.region_tier {
            let Some(entry) = self
                .programs
                .get(handle.native.program)
                .and_then(|program| program.entry(handle.native.entry))
            else {
                return NativeContinuation::Unavailable;
            };
            if remaining >= entry.minimum_instruction_count() as u64 {
                return NativeContinuation::Region(NativeRegionEntry {
                    entry,
                    metadata: &handle.metadata,
                });
            }
            region_budget_fallback = true;
            region_loop_budget_fallback = handle.metadata.is_loop();
        }
        if let BasicTier::Native(handle) = block.basic_tier {
            let Some(entry) = self
                .programs
                .get(handle.program)
                .and_then(|program| program.entry(handle.entry))
            else {
                return NativeContinuation::Unavailable;
            };
            if remaining >= entry.instruction_count() as u64 {
                return NativeContinuation::Basic {
                    entry,
                    source: successor.target,
                    region_budget_fallback,
                    region_loop_budget_fallback,
                };
            }
            return NativeContinuation::Budget;
        }

        if region_budget_fallback {
            NativeContinuation::Budget
        } else {
            NativeContinuation::Unavailable
        }
    }

    #[cfg(test)]
    pub(crate) fn edge_snapshot(&self, id: BlockId) -> Option<EdgeSnapshot> {
        self.blocks
            .get(id.index())
            .map(|block| block.edges.snapshot())
    }

    pub(crate) fn staged_revisit_requires_flush(&mut self, id: BlockId) -> bool {
        if self.staged.is_empty() {
            return false;
        }
        let block = &self.blocks[id.index()];
        if !matches!(block.basic_tier, BasicTier::Staged)
            && !matches!(block.region_tier, RegionTier::Staged)
        {
            return false;
        }
        self.staged_revisits = self.staged_revisits.saturating_add(1);
        self.staged_revisits >= STAGED_REVISIT_FLUSH_INTERVAL
    }

    pub(crate) fn observe_and_compile(
        &mut self,
        id: BlockId,
        code_budget: usize,
        #[cfg(feature = "profile")] profile: &mut ProfileCounters,
    ) -> usize {
        let BasicTier::Profiling(executions) = &mut self.blocks[id.index()].basic_tier else {
            return 0;
        };
        *executions = executions.saturating_add(1);
        if *executions < COMPILATION_THRESHOLD {
            return 0;
        }

        #[cfg(feature = "profile")]
        profile.record_compile_attempt();
        #[cfg(feature = "profile")]
        let compile_started = Instant::now();

        let Some(compiled) = CompiledBlock::compile(self.block(id).instructions()) else {
            self.blocks[id.index()].basic_tier = BasicTier::Disabled;
            self.blocks[id.index()].region_tier = RegionTier::Disabled;
            #[cfg(feature = "profile")]
            profile.record_compile_failure(CompileFailure::NoCode, compile_started.elapsed());
            return 0;
        };
        #[cfg(feature = "profile")]
        let compile_elapsed = compile_started.elapsed();
        #[cfg(feature = "profile")]
        profile.record_compiled_code(compiled.code_len());

        if compiled.instruction_count() < MIN_NATIVE_INSTRUCTIONS {
            self.blocks[id.index()].basic_tier = BasicTier::Disabled;
            self.blocks[id.index()].region_tier = RegionTier::Disabled;
            #[cfg(feature = "profile")]
            profile.record_compile_failure(CompileFailure::TooShort, compile_elapsed);
            return 0;
        }

        self.stage_compiled(
            StagedOwner::Basic(id),
            compiled,
            code_budget,
            #[cfg(feature = "profile")]
            compile_elapsed,
            #[cfg(feature = "profile")]
            profile,
        )
    }

    pub(crate) fn observe_and_compile_region(
        &mut self,
        head: BlockId,
        code_budget: usize,
        #[cfg(feature = "profile")] profile: &mut ProfileCounters,
    ) -> usize {
        self.observe_and_compile_region_with_limits(
            head,
            code_budget,
            VM4_REGION_LIMITS,
            #[cfg(feature = "profile")]
            profile,
        )
    }

    fn observe_and_compile_region_with_limits(
        &mut self,
        head: BlockId,
        code_budget: usize,
        region_limits: RegionLimits,
        #[cfg(feature = "profile")] profile: &mut ProfileCounters,
    ) -> usize {
        let Some(head_block) = self.blocks.get(head.index()) else {
            return 0;
        };
        if !matches!(head_block.region_tier, RegionTier::Profiling)
            || !matches!(head_block.basic_tier, BasicTier::Native(_))
        {
            return 0;
        }
        let path = match self.dominant_region_path(head) {
            RegionPathSelection::Pending => return 0,
            RegionPathSelection::Terminal(_stop) => {
                #[cfg(feature = "profile")]
                profile.record_region_path_stop(
                    matches!(_stop, RegionPathStop::BlockLimit),
                    matches!(_stop, RegionPathStop::InstructionLimit),
                    matches!(_stop, RegionPathStop::Terminal),
                    matches!(_stop, RegionPathStop::Jalr),
                    matches!(_stop, RegionPathStop::ProfileBoundary),
                    matches!(_stop, RegionPathStop::LoopClosure),
                );
                self.blocks[head.index()].region_tier = RegionTier::Disabled;
                return 0;
            }
            RegionPathSelection::Ready(path) => path,
        };

        #[cfg(feature = "profile")]
        {
            profile.record_region_path_selected(path.blocks.len(), path.instruction_count);
            profile.record_region_path_stop(
                matches!(path.stop, RegionPathStop::BlockLimit),
                matches!(path.stop, RegionPathStop::InstructionLimit),
                matches!(path.stop, RegionPathStop::Terminal),
                matches!(path.stop, RegionPathStop::Jalr),
                matches!(path.stop, RegionPathStop::ProfileBoundary),
                matches!(path.stop, RegionPathStop::LoopClosure),
            );
            profile.record_compile_attempt();
            profile.record_region_compile_attempt();
        }
        #[cfg(feature = "profile")]
        let compile_started = Instant::now();

        let loop_compiled = path.head_closing_prefix.and_then(|block_count| {
            #[cfg(feature = "profile")]
            profile.record_loop_compile_attempt();
            let candidate = &path.blocks[..block_count];
            let region_blocks = candidate
                .iter()
                .map(|&id| RegionBlock::new(self.blocks[id.index()].block.instructions()))
                .collect::<Vec<_>>();
            let compiled = CompiledBlock::compile_loop(&region_blocks);
            #[cfg(feature = "profile")]
            if compiled.is_none() {
                profile.record_loop_compile_failures(1);
            }
            compiled.map(|compiled| (compiled, block_count))
        });
        let compiled = loop_compiled.or_else(|| {
            (2..=path.blocks.len()).rev().find_map(|block_count| {
                let candidate = &path.blocks[..block_count];
                let region_blocks = candidate
                    .iter()
                    .map(|&id| RegionBlock::new(self.blocks[id.index()].block.instructions()))
                    .collect::<Vec<_>>();
                let compiled = if self.region_path_repeats_pc(candidate) {
                    CompiledBlock::compile_unrolled_region_with_limits(
                        &region_blocks,
                        region_limits,
                    )
                } else {
                    CompiledBlock::compile_region_with_limits(&region_blocks, region_limits)
                };
                compiled.map(|compiled| (compiled, block_count))
            })
        });
        let Some((compiled, compiled_block_count)) = compiled else {
            self.blocks[head.index()].region_tier = RegionTier::Disabled;
            #[cfg(feature = "profile")]
            {
                profile.record_compile_failure(CompileFailure::NoCode, compile_started.elapsed());
                profile.record_region_compile_failures(1);
            }
            return 0;
        };
        #[cfg(feature = "profile")]
        let compile_elapsed = compile_started.elapsed();
        #[cfg(feature = "profile")]
        {
            profile.record_region_path_compiled(
                compiled_block_count,
                compiled.instruction_count(),
                compiled_block_count < path.blocks.len(),
            );
            profile.record_compiled_code(compiled.code_len());
            profile.record_region_compiled_code(compiled.code_len());
            if matches!(compiled.kind(), NativeEntryKind::Loop) {
                profile.record_loop_compiled_code(compiled.code_len());
            }
        }
        let metadata_path = &path.blocks[..compiled_block_count];
        let shape = match compiled.kind() {
            NativeEntryKind::Bounded => RegionShape::Bounded,
            NativeEntryKind::Loop => RegionShape::Loop {
                head_pc: self.blocks[head.index()].start_pc,
                cycle_instructions: compiled.instruction_count(),
            },
        };
        let metadata = RegionMetadata::new(
            metadata_path,
            &self.blocks,
            compiled.instruction_count(),
            shape,
        );

        self.stage_compiled(
            StagedOwner::Region { head, metadata },
            compiled,
            code_budget,
            #[cfg(feature = "profile")]
            compile_elapsed,
            #[cfg(feature = "profile")]
            profile,
        )
    }

    fn dominant_region_path(&self, head: BlockId) -> RegionPathSelection {
        let mut path = Vec::with_capacity(VM4_REGION_MAX_BLOCKS);
        path.push(head);
        let Some(head_block) = self.blocks.get(head.index()) else {
            return RegionPathSelection::Terminal(RegionPathStop::Terminal);
        };
        let mut instruction_count = head_block.block.len();
        loop {
            let source = *path.last().expect("region paths always contain their head");
            let Some(source_block) = self.blocks.get(source.index()) else {
                return stopped_region_path(
                    path,
                    None,
                    instruction_count,
                    RegionPathStop::Terminal,
                );
            };
            if block_ends_jalr(source_block) {
                return stopped_region_path(path, None, instruction_count, RegionPathStop::Jalr);
            };
            let minimum_observations = if path.len() == 1 {
                DOMINANT_EDGE_MINIMUM_OBSERVATIONS
            } else {
                DOMINANT_PATH_MINIMUM_OBSERVATIONS
            };
            let Some(successor) = source_block.edges.snapshot().dominant_successor(
                minimum_observations,
                DOMINANT_EDGE_NUMERATOR,
                DOMINANT_EDGE_DENOMINATOR,
            ) else {
                return if path.len() == 1 {
                    RegionPathSelection::Pending
                } else {
                    RegionPathSelection::Ready(DominantRegionPath {
                        blocks: path,
                        head_closing_prefix: None,
                        instruction_count,
                        stop: RegionPathStop::ProfileBoundary,
                    })
                };
            };
            let Some(successor_block) = self.blocks.get(successor.target.index()) else {
                return stopped_region_path(
                    path,
                    None,
                    instruction_count,
                    RegionPathStop::Terminal,
                );
            };
            if successor_block.start_pc != successor.target_pc {
                return stopped_region_path(
                    path,
                    None,
                    instruction_count,
                    RegionPathStop::Terminal,
                );
            }
            match &successor_block.basic_tier {
                BasicTier::Native(_) => {}
                BasicTier::Profiling(_) | BasicTier::Staged => {
                    return if path.len() == 1 {
                        RegionPathSelection::Pending
                    } else {
                        RegionPathSelection::Ready(DominantRegionPath {
                            blocks: path,
                            head_closing_prefix: None,
                            instruction_count,
                            stop: RegionPathStop::ProfileBoundary,
                        })
                    };
                }
                BasicTier::Disabled => {
                    return stopped_region_path(
                        path,
                        None,
                        instruction_count,
                        RegionPathStop::Terminal,
                    );
                }
            }
            if successor.target == head {
                let block_count = path.len();
                return RegionPathSelection::Ready(DominantRegionPath {
                    blocks: path,
                    head_closing_prefix: Some(block_count),
                    instruction_count,
                    stop: RegionPathStop::LoopClosure,
                });
            }
            if path.len() == VM4_REGION_MAX_BLOCKS {
                return RegionPathSelection::Ready(DominantRegionPath {
                    blocks: path,
                    head_closing_prefix: None,
                    instruction_count,
                    stop: RegionPathStop::BlockLimit,
                });
            }
            let Some(extended_count) = instruction_count.checked_add(successor_block.block.len())
            else {
                return stopped_region_path(
                    path,
                    None,
                    instruction_count,
                    RegionPathStop::InstructionLimit,
                );
            };
            if extended_count > VM4_REGION_MAX_INSTRUCTIONS {
                return stopped_region_path(
                    path,
                    None,
                    instruction_count,
                    RegionPathStop::InstructionLimit,
                );
            }
            path.push(successor.target);
            instruction_count = extended_count;
        }
    }

    fn region_path_repeats_pc(&self, path: &[BlockId]) -> bool {
        let mut seen = Vec::new();
        for &id in path {
            for instruction in self.blocks[id.index()].block.instructions() {
                let Ok(instruction) = instruction else {
                    continue;
                };
                if seen.contains(&instruction.pc()) {
                    return true;
                }
                seen.push(instruction.pc());
            }
        }
        false
    }

    fn stage_compiled(
        &mut self,
        owner: StagedOwner,
        compiled: CompiledBlock,
        code_budget: usize,
        #[cfg(feature = "profile")] compile_elapsed: Duration,
        #[cfg(feature = "profile")] profile: &mut ProfileCounters,
    ) -> usize {
        let mut mapped_bytes = 0;
        if !self.staged.is_empty()
            && (self.staged.len() == MAX_COHORT_ENTRIES
                || self
                    .staged_code_bytes
                    .checked_add(compiled.code_len())
                    .is_none_or(|bytes| bytes > COHORT_CODE_BYTES))
        {
            mapped_bytes += self.flush_pending(
                code_budget,
                #[cfg(feature = "profile")]
                profile,
            );
        }

        self.staged_code_bytes = self
            .staged_code_bytes
            .checked_add(compiled.code_len())
            .expect("bounded compiled code size cannot overflow");
        match owner {
            StagedOwner::Basic(id) => {
                self.blocks[id.index()].basic_tier = BasicTier::Staged;
            }
            StagedOwner::Region { head, .. } => {
                self.blocks[head.index()].region_tier = RegionTier::Staged;
            }
        }
        self.staged.push(StagedEntry {
            owner,
            compiled,
            #[cfg(feature = "profile")]
            compile_elapsed,
        });

        if self.staged.len() == MAX_COHORT_ENTRIES || self.staged_code_bytes >= COHORT_CODE_BYTES {
            mapped_bytes += self.flush_pending(
                code_budget.saturating_sub(mapped_bytes),
                #[cfg(feature = "profile")]
                profile,
            );
        }
        mapped_bytes
    }

    pub(crate) fn flush_pending(
        &mut self,
        code_budget: usize,
        #[cfg(feature = "profile")] profile: &mut ProfileCounters,
    ) -> usize {
        self.staged_revisits = 0;
        if self.staged.is_empty() {
            return 0;
        }

        let staged = std::mem::take(&mut self.staged);
        self.staged_code_bytes = 0;
        #[cfg(feature = "profile")]
        let compile_elapsed = staged.iter().fold(Duration::ZERO, |elapsed, staged| {
            elapsed.saturating_add(staged.compile_elapsed)
        });
        let owners = staged.iter().map(|staged| staged.owner).collect::<Vec<_>>();
        #[cfg(feature = "profile")]
        let region_count = owners
            .iter()
            .filter(|owner| matches!(owner, StagedOwner::Region { .. }))
            .count();
        #[cfg(feature = "profile")]
        let loop_count = owners
            .iter()
            .filter(|owner| {
                matches!(
                    owner,
                    StagedOwner::Region { metadata, .. } if metadata.is_loop()
                )
            })
            .count();
        let blocks = staged
            .into_iter()
            .map(|staged| staged.compiled)
            .collect::<Vec<_>>();
        #[cfg(feature = "profile")]
        let publish_started = Instant::now();

        let Some(program) = NativeProgram::publish(blocks, code_budget) else {
            for owner in &owners {
                match *owner {
                    StagedOwner::Basic(id) => {
                        self.blocks[id.index()].basic_tier = BasicTier::Disabled;
                        self.blocks[id.index()].region_tier = RegionTier::Disabled;
                    }
                    StagedOwner::Region { head, .. } => {
                        self.blocks[head.index()].region_tier = RegionTier::Disabled;
                    }
                }
            }
            #[cfg(feature = "profile")]
            {
                profile.record_compile_failures(
                    CompileFailure::Publication,
                    owners.len(),
                    compile_elapsed.saturating_add(publish_started.elapsed()),
                );
                profile.record_region_compile_failures(region_count);
                profile.record_loop_compile_failures(loop_count);
            }
            return 0;
        };

        let mapped_bytes = program.mapped_len();
        let program_id = self.programs.len();
        self.programs.push(program);
        for (entry, owner) in owners.iter().copied().enumerate() {
            let native = NativeHandle {
                program: program_id,
                entry,
            };
            match owner {
                StagedOwner::Basic(id) => {
                    self.blocks[id.index()].basic_tier = BasicTier::Native(native);
                }
                StagedOwner::Region { head, metadata } => {
                    self.blocks[head.index()].region_tier =
                        RegionTier::Native(RegionHandle { native, metadata });
                }
            }
        }
        #[cfg(feature = "profile")]
        {
            profile.record_compile_successes(
                owners.len(),
                mapped_bytes,
                compile_elapsed.saturating_add(publish_started.elapsed()),
            );
            profile.record_region_compile_successes(region_count);
            profile.record_loop_compile_successes(loop_count);
        }
        mapped_bytes
    }

    pub(crate) fn get_or_translate(
        &mut self,
        machine: &Machine,
        pc: u32,
        #[cfg(feature = "profile")] profile: &mut ProfileCounters,
    ) -> BlockLookup {
        let page = (pc >> PAGE_SHIFT) as usize;
        let slot = (pc as usize & (PAGE_SIZE - 1)) / 4;
        if let Some(id) = self.pages[page]
            .as_ref()
            .and_then(|page| page.entries[slot])
        {
            #[cfg(feature = "profile")]
            profile.record_cache_hit();
            return BlockLookup::Cached(id);
        }

        #[cfg(feature = "profile")]
        profile.record_cache_miss();
        let block = BasicBlock::translate(machine, pc);
        let decoded = self
            .decoded_instructions
            .checked_add(block.instructions().len());
        if self.blocks.len() == self.maximum_blocks
            || decoded.is_none_or(|count| count > self.maximum_decoded_instructions)
        {
            #[cfg(feature = "profile")]
            profile.record_transient_translation();
            return BlockLookup::Transient(block);
        }

        self.decoded_instructions = decoded.expect("cache limit prevents overflow");
        let id = BlockId::new(self.blocks.len());
        self.blocks.push(CachedBlock::new(pc, block));
        let page = self.pages[page].get_or_insert_with(|| Box::new(BlockPage::new()));
        page.entries[slot] = Some(id);
        BlockLookup::Cached(id)
    }

    #[cfg(test)]
    pub(crate) fn block_count(&self) -> usize {
        self.blocks.len()
    }

    #[cfg(test)]
    pub(crate) fn cached_block_id(&self, pc: u32) -> Option<BlockId> {
        if pc & 3 != 0 || pc >= (PAGE_COUNT * PAGE_SIZE) as u32 {
            return None;
        }
        let page = (pc >> PAGE_SHIFT) as usize;
        let slot = (pc as usize & (PAGE_SIZE - 1)) / 4;
        self.pages[page]
            .as_ref()
            .and_then(|page| page.entries[slot])
    }

    #[cfg(all(
        test,
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    pub(crate) fn native_block_count(&self) -> usize {
        self.blocks
            .iter()
            .filter(|cached| matches!(cached.basic_tier, BasicTier::Native(_)))
            .count()
    }

    #[cfg(all(
        test,
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    pub(crate) fn native_region_count(&self) -> usize {
        self.blocks
            .iter()
            .filter(|cached| matches!(cached.region_tier, RegionTier::Native(_)))
            .count()
    }

    #[cfg(all(
        test,
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    pub(crate) fn native_loop_count(&self) -> usize {
        self.blocks
            .iter()
            .filter(|cached| {
                matches!(
                    &cached.region_tier,
                    RegionTier::Native(handle) if handle.metadata.is_loop()
                )
            })
            .count()
    }

    #[cfg(all(
        test,
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    pub(crate) fn program_count(&self) -> usize {
        self.programs.len()
    }

    #[cfg(all(
        test,
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    pub(crate) fn staged_block_count(&self) -> usize {
        self.staged.len()
    }
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::{machine::Machine, memory::IMAGE_START};
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use rv32vm_rust_x86_block_compiler::{
        CompiledBlock, NativeEntryKind, RegionBlock, RegionLimits,
    };

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use super::DOMINANT_EDGE_MINIMUM_OBSERVATIONS;
    use super::{
        BasicTier, BlockCache, BlockId, BlockLookup, MAX_REGION_EDGE_OBSERVATIONS,
        NativeContinuation, RegionTier, STAGED_REVISIT_FLUSH_INTERVAL,
    };
    #[cfg(feature = "profile")]
    use crate::profile::ProfileCounters as TestProfile;
    use crate::test_support::{NOP, machine_with_code_at};
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use crate::test_support::{addi, beq, lw};
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    use rv32vm_rust_common::memory::PAGE_SIZE;

    #[cfg(not(feature = "profile"))]
    #[derive(Default)]
    struct TestProfile;

    fn test_profile() -> TestProfile {
        #[cfg(feature = "profile")]
        return TestProfile::default();
        #[cfg(not(feature = "profile"))]
        TestProfile
    }

    fn observe_and_compile(
        cache: &mut BlockCache,
        id: BlockId,
        code_budget: usize,
        profile: &mut TestProfile,
    ) -> usize {
        #[cfg(feature = "profile")]
        return cache.observe_and_compile(id, code_budget, profile);
        #[cfg(not(feature = "profile"))]
        {
            let _ = profile;
            cache.observe_and_compile(id, code_budget)
        }
    }

    fn flush_pending(
        cache: &mut BlockCache,
        code_budget: usize,
        profile: &mut TestProfile,
    ) -> usize {
        #[cfg(feature = "profile")]
        return cache.flush_pending(code_budget, profile);
        #[cfg(not(feature = "profile"))]
        {
            let _ = profile;
            cache.flush_pending(code_budget)
        }
    }

    fn get_or_translate(
        cache: &mut BlockCache,
        machine: &Machine,
        pc: u32,
        profile: &mut TestProfile,
    ) -> BlockLookup {
        #[cfg(feature = "profile")]
        return cache.get_or_translate(machine, pc, profile);
        #[cfg(not(feature = "profile"))]
        {
            let _ = profile;
            cache.get_or_translate(machine, pc)
        }
    }

    fn cached_id(
        cache: &mut BlockCache,
        machine: &Machine,
        pc: u32,
        profile: &mut TestProfile,
    ) -> BlockId {
        let BlockLookup::Cached(id) = get_or_translate(cache, machine, pc, profile) else {
            panic!("test block should be cached");
        };
        id
    }

    fn observe_edge(
        cache: &mut BlockCache,
        source: BlockId,
        target_pc: u32,
        target: BlockId,
        profile: &mut TestProfile,
    ) -> bool {
        #[cfg(feature = "profile")]
        return cache.observe_edge(source, target_pc, target, profile);
        #[cfg(not(feature = "profile"))]
        {
            let _ = profile;
            cache.observe_edge(source, target_pc, target)
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    fn observe_and_compile_region(
        cache: &mut BlockCache,
        id: BlockId,
        code_budget: usize,
        profile: &mut TestProfile,
    ) -> usize {
        #[cfg(feature = "profile")]
        return cache.observe_and_compile_region(id, code_budget, profile);
        #[cfg(not(feature = "profile"))]
        {
            let _ = profile;
            cache.observe_and_compile_region(id, code_budget)
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    fn observe_and_compile_region_with_limits(
        cache: &mut BlockCache,
        id: BlockId,
        code_budget: usize,
        limits: RegionLimits,
        profile: &mut TestProfile,
    ) -> usize {
        #[cfg(feature = "profile")]
        return cache.observe_and_compile_region_with_limits(id, code_budget, limits, profile);
        #[cfg(not(feature = "profile"))]
        {
            let _ = profile;
            cache.observe_and_compile_region_with_limits(id, code_budget, limits)
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    fn heat(cache: &mut BlockCache, id: BlockId, budget: usize, profile: &mut TestProfile) {
        for _ in 0..3 {
            let _ = observe_and_compile(cache, id, budget, profile);
        }
    }

    #[test]
    fn attempts_compilation_after_three_completed_executions() {
        let machine = machine_with_code_at(&[0x0000_0073], IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let id = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);

        assert_eq!(
            observe_and_compile(&mut cache, id, usize::MAX, &mut profile),
            0
        );
        assert!(matches!(
            cache.blocks[id.index()].basic_tier,
            BasicTier::Profiling(1)
        ));
        assert_eq!(
            observe_and_compile(&mut cache, id, usize::MAX, &mut profile),
            0
        );
        assert!(matches!(
            cache.blocks[id.index()].basic_tier,
            BasicTier::Profiling(2)
        ));
        assert_eq!(
            observe_and_compile(&mut cache, id, usize::MAX, &mut profile),
            0
        );
        assert!(matches!(
            cache.blocks[id.index()].basic_tier,
            BasicTier::Disabled
        ));
        assert!(matches!(
            cache.blocks[id.index()].region_tier,
            RegionTier::Disabled
        ));
        assert!(!cache.profiles_edges(id));
    }

    #[test]
    fn full_cache_uses_transient_blocks_without_growing() {
        let machine = machine_with_code_at(&[NOP; 65], IMAGE_START);
        let mut cache = BlockCache::with_limits(1, usize::MAX);
        let mut profile = test_profile();

        assert!(matches!(
            get_or_translate(&mut cache, &machine, IMAGE_START, &mut profile),
            BlockLookup::Cached(_)
        ));
        assert!(matches!(
            get_or_translate(&mut cache, &machine, IMAGE_START + 4, &mut profile),
            BlockLookup::Transient(_)
        ));
        assert_eq!(cache.block_count(), 1);
    }

    #[test]
    fn decoded_instruction_limit_uses_transient_blocks() {
        let machine = machine_with_code_at(&[0x0000_0063, NOP], IMAGE_START);
        let mut cache = BlockCache::with_limits(usize::MAX, 1);
        let mut profile = test_profile();

        assert!(matches!(
            get_or_translate(&mut cache, &machine, IMAGE_START, &mut profile),
            BlockLookup::Cached(_)
        ));
        assert!(matches!(
            get_or_translate(&mut cache, &machine, IMAGE_START + 4, &mut profile),
            BlockLookup::Transient(_)
        ));
        assert_eq!(cache.block_count(), 1);
    }

    #[test]
    fn self_loop_edge_counts_saturate() {
        let machine = machine_with_code_at(&[0x0000_0063], IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let source = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);

        assert!(observe_edge(
            &mut cache,
            source,
            IMAGE_START,
            source,
            &mut profile,
        ));
        let snapshot = cache.edge_snapshot(source).unwrap();
        assert_eq!(snapshot.observations, 1);
        assert_eq!(snapshot.successors[0].unwrap().observations, 1);

        cache.blocks[source.index()].edges.observations = u32::MAX;
        cache.blocks[source.index()].edges.successors[0]
            .as_mut()
            .unwrap()
            .observations = u32::MAX;
        assert!(observe_edge(
            &mut cache,
            source,
            IMAGE_START,
            source,
            &mut profile,
        ));
        let snapshot = cache.edge_snapshot(source).unwrap();
        assert_eq!(snapshot.observations, u32::MAX);
        assert_eq!(snapshot.successors[0].unwrap().observations, u32::MAX);
    }

    #[test]
    fn conditional_edges_retain_two_exact_successors_and_dominance() {
        let machine = machine_with_code_at(&[0x0000_0063; 3], IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let source = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        let fallthrough = cached_id(&mut cache, &machine, IMAGE_START + 4, &mut profile);
        let taken = cached_id(&mut cache, &machine, IMAGE_START + 8, &mut profile);

        for _ in 0..3 {
            assert!(observe_edge(
                &mut cache,
                source,
                IMAGE_START + 4,
                fallthrough,
                &mut profile,
            ));
        }
        assert!(observe_edge(
            &mut cache,
            source,
            IMAGE_START + 8,
            taken,
            &mut profile,
        ));

        let snapshot = cache.edge_snapshot(source).unwrap();
        assert_eq!(snapshot.observations, 4);
        assert_eq!(snapshot.successors[0].unwrap().target, fallthrough);
        assert_eq!(snapshot.successors[1].unwrap().target, taken);
        assert_eq!(
            snapshot.dominant_successor(4, 3, 4).unwrap().target,
            fallthrough
        );
        assert!(snapshot.dominant_successor(4, 4, 4).is_none());
    }

    #[test]
    fn polymorphic_edges_replace_the_first_tied_slot_deterministically() {
        let machine = machine_with_code_at(&[0x0000_0063; 4], IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let source = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        let first = cached_id(&mut cache, &machine, IMAGE_START + 4, &mut profile);
        let second = cached_id(&mut cache, &machine, IMAGE_START + 8, &mut profile);
        let third = cached_id(&mut cache, &machine, IMAGE_START + 12, &mut profile);

        assert!(observe_edge(
            &mut cache,
            source,
            IMAGE_START + 4,
            first,
            &mut profile,
        ));
        assert!(observe_edge(
            &mut cache,
            source,
            IMAGE_START + 8,
            second,
            &mut profile,
        ));
        assert!(observe_edge(
            &mut cache,
            source,
            IMAGE_START + 12,
            third,
            &mut profile,
        ));
        assert!(observe_edge(
            &mut cache,
            source,
            IMAGE_START + 12,
            third,
            &mut profile,
        ));

        let snapshot = cache.edge_snapshot(source).unwrap();
        assert_eq!(snapshot.observations, 4);
        assert_eq!(snapshot.successors[0].unwrap().target, third);
        assert_eq!(snapshot.successors[0].unwrap().observations, 2);
        assert_eq!(snapshot.successors[1].unwrap().target, second);
        #[cfg(feature = "profile")]
        {
            assert_eq!(profile.edge_observations(), 4);
            assert_eq!(profile.edge_profile_hits(), 1);
            assert_eq!(profile.edge_profile_replacements(), 1);
        }
    }

    #[test]
    fn terminal_region_sources_reject_edge_observations() {
        let machine = machine_with_code_at(&[0x0000_0063; 2], IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let source = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        let target = cached_id(&mut cache, &machine, IMAGE_START + 4, &mut profile);
        cache.blocks[source.index()].region_tier = RegionTier::Disabled;

        assert!(!cache.profiles_edges(source));
        assert!(!observe_edge(
            &mut cache,
            source,
            IMAGE_START + 4,
            target,
            &mut profile,
        ));
        assert_eq!(cache.edge_snapshot(source).unwrap().observations, 0);
        #[cfg(feature = "profile")]
        assert_eq!(profile.edge_observations(), 0);
    }

    #[test]
    fn frozen_successors_require_exact_pc_and_validated_block_ids() {
        let machine = machine_with_code_at(&[0x0000_0063; 3], IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let source = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        let target = cached_id(&mut cache, &machine, IMAGE_START + 4, &mut profile);
        assert!(observe_edge(
            &mut cache,
            source,
            IMAGE_START + 4,
            target,
            &mut profile,
        ));

        assert!(matches!(
            cache.native_continuation(source, IMAGE_START + 4, u64::MAX),
            NativeContinuation::Profiling
        ));
        cache.blocks[source.index()].region_tier = RegionTier::Disabled;
        assert!(matches!(
            cache.native_continuation(source, IMAGE_START + 4, u64::MAX),
            NativeContinuation::Unavailable
        ));
        assert!(matches!(
            cache.native_continuation(source, IMAGE_START + 8, u64::MAX),
            NativeContinuation::Miss
        ));

        cache.blocks[target.index()].start_pc = IMAGE_START + 8;
        assert!(matches!(
            cache.native_continuation(source, IMAGE_START + 4, u64::MAX),
            NativeContinuation::Miss
        ));
    }

    #[test]
    fn nondominant_edges_freeze_at_the_exact_profile_bound() {
        let machine = machine_with_code_at(&[0x0000_0063; 3], IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let source = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        let first = cached_id(&mut cache, &machine, IMAGE_START + 4, &mut profile);
        let second = cached_id(&mut cache, &machine, IMAGE_START + 8, &mut profile);

        for observation in 0..MAX_REGION_EDGE_OBSERVATIONS {
            let (target_pc, target) = if observation % 2 == 0 {
                (IMAGE_START + 4, first)
            } else {
                (IMAGE_START + 8, second)
            };
            assert!(observe_edge(
                &mut cache,
                source,
                target_pc,
                target,
                &mut profile,
            ));
        }

        let snapshot = cache.edge_snapshot(source).unwrap();
        assert_eq!(snapshot.observations, MAX_REGION_EDGE_OBSERVATIONS);
        assert_eq!(snapshot.successors[0].unwrap().observations, 32);
        assert_eq!(snapshot.successors[1].unwrap().observations, 32);
        assert!(matches!(
            cache.blocks[source.index()].region_tier,
            RegionTier::Disabled
        ));
        assert!(!cache.profiles_edges(source));
        assert!(!observe_edge(
            &mut cache,
            source,
            IMAGE_START + 4,
            first,
            &mut profile,
        ));
        assert_eq!(
            cache.edge_snapshot(source).unwrap().observations,
            MAX_REGION_EDGE_OBSERVATIONS
        );
        #[cfg(feature = "profile")]
        assert_eq!(
            profile.edge_observations(),
            u64::from(MAX_REGION_EDGE_OBSERVATIONS)
        );
    }

    #[test]
    fn clear_discards_edges_and_transient_targets_are_not_recorded() {
        let machine = machine_with_code_at(&[0x0000_0063; 2], IMAGE_START);
        let mut cache = BlockCache::with_limits(1, usize::MAX);
        let mut profile = test_profile();
        let source = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        assert!(matches!(
            get_or_translate(&mut cache, &machine, IMAGE_START + 4, &mut profile),
            BlockLookup::Transient(_)
        ));
        assert!(!observe_edge(
            &mut cache,
            source,
            IMAGE_START + 4,
            source,
            &mut profile,
        ));
        assert_eq!(cache.edge_snapshot(source).unwrap().observations, 0);

        assert!(observe_edge(
            &mut cache,
            source,
            IMAGE_START,
            source,
            &mut profile,
        ));
        cache.clear();
        assert!(cache.edge_snapshot(source).is_none());

        let source = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        assert_eq!(cache.edge_snapshot(source).unwrap().observations, 0);
    }

    #[test]
    fn staged_revisit_interval_resets_on_flush_and_clear() {
        let machine = machine_with_code_at(&[0x0000_0063], IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let id = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        for _ in 0..3 {
            let _ = observe_and_compile(&mut cache, id, usize::MAX, &mut profile);
        }
        assert!(!cache.staged.is_empty());
        assert!(matches!(
            cache.blocks[id.index()].basic_tier,
            BasicTier::Staged
        ));

        for _ in 1..STAGED_REVISIT_FLUSH_INTERVAL {
            assert!(!cache.staged_revisit_requires_flush(id));
        }
        assert!(cache.staged_revisit_requires_flush(id));

        let _ = flush_pending(&mut cache, usize::MAX, &mut profile);
        assert_eq!(cache.staged_revisits, 0);

        cache.staged_revisits = STAGED_REVISIT_FLUSH_INTERVAL;
        cache.clear();
        assert_eq!(cache.staged_revisits, 0);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn zero_code_budget_disables_native_compilation() {
        let machine =
            machine_with_code_at(&[addi(5, 0, 1), addi(5, 5, 1), lw(6, 0, 0)], IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let id = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);

        heat(&mut cache, id, 0, &mut profile);
        assert!(matches!(
            cache.blocks[id.index()].basic_tier,
            BasicTier::Staged
        ));
        assert_eq!(flush_pending(&mut cache, 0, &mut profile), 0);

        assert!(matches!(
            cache.blocks[id.index()].basic_tier,
            BasicTier::Disabled
        ));
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn native_continuation_status_stops_for_staging_and_exact_budgets() {
        let machine = machine_with_code_at(
            &[addi(5, 5, 1), 0x0000_0063, addi(6, 6, 1), 0x0000_0063],
            IMAGE_START,
        );
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let source = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        let target = cached_id(&mut cache, &machine, IMAGE_START + 8, &mut profile);
        assert!(observe_edge(
            &mut cache,
            source,
            IMAGE_START + 8,
            target,
            &mut profile,
        ));
        cache.blocks[source.index()].region_tier = RegionTier::Disabled;

        assert!(matches!(
            cache.native_continuation(source, IMAGE_START + 8, u64::MAX),
            NativeContinuation::Unavailable
        ));
        heat(&mut cache, target, usize::MAX, &mut profile);
        assert!(matches!(
            cache.native_continuation(source, IMAGE_START + 8, u64::MAX),
            NativeContinuation::Unavailable
        ));

        assert_ne!(flush_pending(&mut cache, usize::MAX, &mut profile), 0);
        assert!(matches!(
            cache.native_continuation(source, IMAGE_START + 8, 1),
            NativeContinuation::Budget
        ));
        assert!(matches!(
            cache.native_continuation(source, IMAGE_START + 8, 2),
            NativeContinuation::Basic {
                source: continuation_source,
                ..
            } if continuation_source == target
        ));
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn publishes_hot_single_instruction_control_flow() {
        let machine = machine_with_code_at(&[0x0000_0063], IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let id = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);

        heat(&mut cache, id, usize::MAX, &mut profile);
        assert_eq!(cache.staged_block_count(), 1);
        assert_eq!(cache.program_count(), 0);
        let mapped = flush_pending(&mut cache, usize::MAX, &mut profile);

        assert!(mapped > 0);
        assert_eq!(cache.staged_block_count(), 0);
        assert!(matches!(
            cache.blocks[id.index()].basic_tier,
            BasicTier::Native(_)
        ));
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn one_program_executes_multiple_staged_entries() {
        let code = [addi(5, 0, 11), beq(0, 0, 8), addi(6, 0, 22), beq(0, 0, 0)];
        let mut machine = machine_with_code_at(&code, IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let first = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        let second = cached_id(&mut cache, &machine, IMAGE_START + 8, &mut profile);
        heat(&mut cache, first, usize::MAX, &mut profile);
        heat(&mut cache, second, usize::MAX, &mut profile);

        assert_eq!(cache.staged_block_count(), 2);
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );
        assert_eq!(cache.program_count(), 1);
        assert_eq!(cache.native_block_count(), 2);

        let first_entry = cache.native_entry(first).unwrap();
        let memory = machine.memory.direct_memory();
        let first_outcome = first_entry.execute(&mut machine.registers, memory);
        assert_eq!(first_outcome.retired(), 2);
        assert_eq!(machine.registers[5], 11);

        let second_entry = cache.native_entry(second).unwrap();
        let memory = machine.memory.direct_memory();
        let second_outcome = second_entry.execute(&mut machine.registers, memory);
        assert_eq!(second_outcome.retired(), 2);
        assert_eq!(machine.registers[6], 22);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn cohort_publication_failure_is_atomic_at_the_exact_budget() {
        let code = [addi(5, 0, 1), beq(0, 0, 8), addi(6, 0, 2), beq(0, 0, 0)];
        let machine = machine_with_code_at(&code, IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let first = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        let second = cached_id(&mut cache, &machine, IMAGE_START + 8, &mut profile);
        heat(&mut cache, first, usize::MAX, &mut profile);
        heat(&mut cache, second, usize::MAX, &mut profile);

        assert_eq!(flush_pending(&mut cache, PAGE_SIZE - 1, &mut profile), 0);
        assert_eq!(cache.program_count(), 0);
        assert_eq!(cache.staged_block_count(), 0);
        assert!(matches!(
            cache.blocks[first.index()].basic_tier,
            BasicTier::Disabled
        ));
        assert!(matches!(
            cache.blocks[second.index()].basic_tier,
            BasicTier::Disabled
        ));
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn regions_wait_for_native_basics_and_share_mixed_cohorts() {
        let code = [
            beq(0, 0, 8),
            beq(0, 0, 0),
            addi(5, 5, 1),
            beq(0, 0, 0),
            addi(6, 6, 1),
            beq(0, 0, 0),
        ];
        let machine = machine_with_code_at(&code, IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let head = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        let successor = cached_id(&mut cache, &machine, IMAGE_START + 8, &mut profile);
        let unrelated = cached_id(&mut cache, &machine, IMAGE_START + 16, &mut profile);

        for _ in 0..7 {
            assert!(observe_edge(
                &mut cache,
                head,
                IMAGE_START + 8,
                successor,
                &mut profile,
            ));
            assert_eq!(
                observe_and_compile_region(&mut cache, head, usize::MAX, &mut profile),
                0
            );
        }
        assert!(matches!(
            cache.blocks[head.index()].region_tier,
            RegionTier::Profiling
        ));

        heat(&mut cache, head, usize::MAX, &mut profile);
        heat(&mut cache, successor, usize::MAX, &mut profile);
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );
        assert_eq!(cache.native_block_count(), 2);
        assert_eq!(cache.native_region_count(), 0);

        heat(&mut cache, unrelated, usize::MAX, &mut profile);
        assert_eq!(cache.staged_block_count(), 1);
        assert!(observe_edge(
            &mut cache,
            head,
            IMAGE_START + 8,
            successor,
            &mut profile,
        ));
        let snapshot = cache.edge_snapshot(head).unwrap();
        assert_eq!(snapshot.observations, 8);
        assert_eq!(
            snapshot.dominant_successor(8, 7, 8).unwrap().target,
            successor
        );
        assert_eq!(
            observe_and_compile_region(&mut cache, head, usize::MAX, &mut profile),
            0
        );
        assert_eq!(cache.staged_block_count(), 2);
        assert!(matches!(
            cache.blocks[head.index()].region_tier,
            RegionTier::Staged
        ));
        assert_eq!(
            observe_and_compile_region(&mut cache, head, usize::MAX, &mut profile),
            0
        );
        assert_eq!(cache.staged_block_count(), 2);

        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );
        assert_eq!(cache.program_count(), 2);
        assert_eq!(cache.native_block_count(), 3);
        assert_eq!(cache.native_region_count(), 1);
        let region = cache.native_region_entry(head).unwrap();
        assert_eq!(region.entry.instruction_count(), 3);
        assert_eq!(region.metadata.block_count(), 2);
        assert_eq!(
            region
                .metadata
                .source_for_retired(region.entry.instruction_count()),
            Some(successor)
        );
        #[cfg(feature = "profile")]
        {
            assert_eq!(profile.region_compile_attempts(), 1);
            assert_eq!(profile.region_compile_successes(), 1);
            assert_eq!(profile.region_compile_failures(), 0);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn eight_block_bounded_regions_preserve_budgets_guards_and_source_boundaries() {
        let mut code = Vec::new();
        for index in 0..9_u32 {
            code.push(addi(5, 5, 1));
            if index + 1 == 9 {
                code.push(0x0000_8067); // jalr x0, x1, 0
            } else {
                code.push(beq(6 + index, 0, 8));
            }
            code.push(NOP);
        }
        let mut machine = machine_with_code_at(&code, IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let blocks = (0..9_u32)
            .map(|index| cached_id(&mut cache, &machine, IMAGE_START + index * 12, &mut profile))
            .collect::<Vec<_>>();
        for &block in &blocks {
            heat(&mut cache, block, usize::MAX, &mut profile);
        }
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        for index in 0..8 {
            let observations = if index == 0 {
                DOMINANT_EDGE_MINIMUM_OBSERVATIONS
            } else {
                DOMINANT_EDGE_MINIMUM_OBSERVATIONS - 1
            };
            for _ in 0..observations {
                assert!(observe_edge(
                    &mut cache,
                    blocks[index],
                    IMAGE_START + (index as u32 + 1) * 12,
                    blocks[index + 1],
                    &mut profile,
                ));
            }
        }
        assert_eq!(
            observe_and_compile_region(&mut cache, blocks[0], usize::MAX, &mut profile),
            0
        );
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        let region = cache.native_region_entry(blocks[0]).unwrap();
        assert_eq!(region.entry.kind(), NativeEntryKind::Bounded);
        assert_eq!(region.entry.instruction_count(), 16);
        assert_eq!(region.entry.minimum_instruction_count(), 16);
        assert_eq!(region.metadata.block_count(), 8);
        for (index, &block) in blocks[..8].iter().enumerate() {
            assert_eq!(
                region.metadata.source_for_retired((index + 1) * 2),
                Some(block)
            );
        }

        let mut registers = [0_u32; 32];
        assert!(
            region
                .entry
                .execute_with_limit(&mut registers, machine.memory.direct_memory(), 15)
                .is_none()
        );
        assert_eq!(registers[5], 0);

        let outcome = region
            .entry
            .execute_with_limit(&mut registers, machine.memory.direct_memory(), 16)
            .unwrap();
        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 16);
        assert_eq!(outcome.next_pc(), IMAGE_START + 8 * 12);
        assert_eq!(registers[5], 8);

        let mut guarded = [0_u32; 32];
        guarded[10] = 1;
        let outcome = region
            .entry
            .execute_with_limit(&mut guarded, machine.memory.direct_memory(), 16)
            .unwrap();
        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 10);
        assert_eq!(outcome.next_pc(), IMAGE_START + 4 * 12 + 8);
        assert_eq!(region.metadata.source_for_retired(10), Some(blocks[4]));
        assert_eq!(guarded[5], 5);

        #[cfg(feature = "profile")]
        {
            assert_eq!(profile.region_paths_selected(), 1);
            assert_eq!(profile.region_selected_blocks(), 8);
            assert_eq!(profile.region_selected_instructions(), 16);
            assert_eq!(profile.region_compiled_blocks(), 8);
            assert_eq!(profile.region_compiled_instructions(), 16);
            assert_eq!(profile.region_path_block_limit_stops(), 1);
            assert_eq!(profile.region_path_prefix_fallbacks(), 0);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn bounded_region_code_limit_falls_back_to_the_longest_safe_prefix() {
        let code = [beq(0, 0, 8), NOP, beq(0, 0, 8), NOP, 0x0000_8067];
        let machine = machine_with_code_at(&code, IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let blocks = [
            cached_id(&mut cache, &machine, IMAGE_START, &mut profile),
            cached_id(&mut cache, &machine, IMAGE_START + 8, &mut profile),
            cached_id(&mut cache, &machine, IMAGE_START + 16, &mut profile),
        ];
        for block in blocks {
            heat(&mut cache, block, usize::MAX, &mut profile);
        }
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );
        for index in 0..2 {
            let observations = if index == 0 {
                DOMINANT_EDGE_MINIMUM_OBSERVATIONS
            } else {
                DOMINANT_EDGE_MINIMUM_OBSERVATIONS - 1
            };
            for _ in 0..observations {
                assert!(observe_edge(
                    &mut cache,
                    blocks[index],
                    IMAGE_START + (index as u32 + 1) * 8,
                    blocks[index + 1],
                    &mut profile,
                ));
            }
        }

        let region_blocks = blocks
            .iter()
            .map(|block| RegionBlock::new(cache.block(*block).instructions()))
            .collect::<Vec<_>>();
        let two_block_bytes = CompiledBlock::compile_region_with_limits(
            &region_blocks[..2],
            RegionLimits::new(8, 256, usize::MAX),
        )
        .unwrap()
        .code_len();
        assert!(
            CompiledBlock::compile_region_with_limits(
                &region_blocks,
                RegionLimits::new(8, 256, two_block_bytes),
            )
            .is_none()
        );

        assert_eq!(
            observe_and_compile_region_with_limits(
                &mut cache,
                blocks[0],
                usize::MAX,
                RegionLimits::new(8, 256, two_block_bytes),
                &mut profile,
            ),
            0
        );
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );
        let region = cache.native_region_entry(blocks[0]).unwrap();
        assert_eq!(region.entry.kind(), NativeEntryKind::Bounded);
        assert_eq!(region.metadata.block_count(), 2);
        assert_eq!(region.entry.instruction_count(), 2);
        #[cfg(feature = "profile")]
        assert_eq!(profile.region_path_prefix_fallbacks(), 1);
    }

    #[cfg(all(
        feature = "profile",
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn bounded_path_profile_records_the_256_instruction_cap() {
        let mut code = Vec::new();
        for _ in 0..5 {
            code.extend(std::iter::repeat_n(NOP, 63));
            code.push(beq(0, 0, 4));
        }
        let machine = machine_with_code_at(&code, IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let blocks = (0..5_u32)
            .map(|index| {
                cached_id(
                    &mut cache,
                    &machine,
                    IMAGE_START + index * 64 * 4,
                    &mut profile,
                )
            })
            .collect::<Vec<_>>();
        for &block in &blocks {
            heat(&mut cache, block, usize::MAX, &mut profile);
        }
        let _ = flush_pending(&mut cache, usize::MAX, &mut profile);
        for index in 0..4 {
            let observations = if index == 0 {
                DOMINANT_EDGE_MINIMUM_OBSERVATIONS
            } else {
                DOMINANT_EDGE_MINIMUM_OBSERVATIONS - 1
            };
            for _ in 0..observations {
                assert!(observe_edge(
                    &mut cache,
                    blocks[index],
                    IMAGE_START + (index as u32 + 1) * 64 * 4,
                    blocks[index + 1],
                    &mut profile,
                ));
            }
        }

        let _ = observe_and_compile_region(&mut cache, blocks[0], usize::MAX, &mut profile);
        assert_eq!(profile.region_paths_selected(), 1);
        assert_eq!(profile.region_selected_blocks(), 4);
        assert_eq!(profile.region_selected_instructions(), 256);
        assert_eq!(profile.region_path_instruction_limit_stops(), 1);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn disabled_first_successors_terminate_dominant_region_profiles() {
        let code = [beq(0, 0, 8), beq(0, 0, 0), 0x0000_0073];
        let machine = machine_with_code_at(&code, IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let head = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        let disabled = cached_id(&mut cache, &machine, IMAGE_START + 8, &mut profile);
        heat(&mut cache, head, usize::MAX, &mut profile);
        heat(&mut cache, disabled, usize::MAX, &mut profile);
        assert!(matches!(
            cache.blocks[disabled.index()].basic_tier,
            BasicTier::Disabled
        ));
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        for _ in 0..DOMINANT_EDGE_MINIMUM_OBSERVATIONS {
            assert!(observe_edge(
                &mut cache,
                head,
                IMAGE_START + 8,
                disabled,
                &mut profile,
            ));
        }
        assert_eq!(
            observe_and_compile_region(&mut cache, head, usize::MAX, &mut profile),
            0
        );

        assert!(matches!(
            cache.blocks[head.index()].region_tier,
            RegionTier::Disabled
        ));
        assert!(!cache.profiles_edges(head));
        assert!(cache.native_entry(head).is_some());
        assert_eq!(
            cache.edge_snapshot(head).unwrap().observations,
            DOMINANT_EDGE_MINIMUM_OBSERVATIONS
        );
        assert!(!observe_edge(
            &mut cache,
            head,
            IMAGE_START + 8,
            disabled,
            &mut profile,
        ));
        #[cfg(feature = "profile")]
        {
            assert_eq!(profile.region_compile_attempts(), 0);
            assert_eq!(profile.region_path_terminal_stops(), 1);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn self_loop_regions_use_counted_native_loops_with_exact_source_boundaries() {
        let code = [addi(5, 5, 1), beq(0, 0, -4)];
        let mut machine = machine_with_code_at(&code, IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let head = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        heat(&mut cache, head, usize::MAX, &mut profile);
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        for _ in 0..DOMINANT_EDGE_MINIMUM_OBSERVATIONS {
            assert!(observe_edge(
                &mut cache,
                head,
                IMAGE_START,
                head,
                &mut profile,
            ));
        }
        assert_eq!(
            observe_and_compile_region(&mut cache, head, usize::MAX, &mut profile),
            0
        );
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        let region = cache.native_region_entry(head).unwrap();
        assert_eq!(cache.native_loop_count(), 1);
        assert_eq!(region.entry.kind(), NativeEntryKind::Loop);
        assert_eq!(region.metadata.block_count(), 1);
        assert_eq!(region.entry.instruction_count(), code.len());
        assert_eq!(region.entry.minimum_instruction_count(), code.len());
        assert_eq!(region.entry.loop_unroll_factor(), 1);
        for cycle in 1..=4 {
            assert_eq!(
                region.metadata.source_for_retired(cycle * code.len()),
                Some(head)
            );
        }

        let memory = machine.memory.direct_memory();
        let budget = 4 * code.len() as u64;
        let outcome = region
            .entry
            .execute_with_limit(&mut machine.registers, memory, budget)
            .unwrap();
        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired() as u64, budget);
        assert_eq!(outcome.next_pc(), IMAGE_START);
        assert_eq!(machine.registers[5], 4);
        #[cfg(feature = "profile")]
        {
            assert_eq!(profile.loop_compile_attempts(), 1);
            assert_eq!(profile.loop_compile_successes(), 1);
            assert_eq!(profile.loop_compile_failures(), 0);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn two_block_cycles_use_counted_native_loops_and_attribute_each_boundary() {
        let code = [addi(5, 5, 1), beq(0, 0, 4), addi(6, 6, 1), beq(0, 0, -12)];
        let mut machine = machine_with_code_at(&code, IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let first = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        let second = cached_id(&mut cache, &machine, IMAGE_START + 8, &mut profile);
        heat(&mut cache, first, usize::MAX, &mut profile);
        heat(&mut cache, second, usize::MAX, &mut profile);
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        for _ in 1..DOMINANT_EDGE_MINIMUM_OBSERVATIONS {
            assert!(observe_edge(
                &mut cache,
                second,
                IMAGE_START,
                first,
                &mut profile,
            ));
        }
        for _ in 0..DOMINANT_EDGE_MINIMUM_OBSERVATIONS {
            assert!(observe_edge(
                &mut cache,
                first,
                IMAGE_START + 8,
                second,
                &mut profile,
            ));
        }
        assert_eq!(
            observe_and_compile_region(&mut cache, first, usize::MAX, &mut profile),
            0
        );
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        let region = cache.native_region_entry(first).unwrap();
        assert_eq!(cache.native_loop_count(), 1);
        assert_eq!(region.entry.kind(), NativeEntryKind::Loop);
        assert_eq!(region.metadata.block_count(), 2);
        assert_eq!(region.entry.instruction_count(), 4);
        assert_eq!(region.entry.minimum_instruction_count(), 4);
        assert_eq!(region.entry.loop_unroll_factor(), 1);
        for copy in 0..4 {
            assert_eq!(
                region.metadata.source_for_retired(copy * 4 + 2),
                Some(first)
            );
            assert_eq!(
                region.metadata.source_for_retired(copy * 4 + 4),
                Some(second)
            );
        }

        let mut registers = [0_u32; 32];
        let memory = machine.memory.direct_memory();
        let outcome = region
            .entry
            .execute_with_limit(&mut registers, memory, 16)
            .unwrap();
        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 16);
        assert_eq!(outcome.next_pc(), IMAGE_START);
        assert_eq!(registers[5], 4);
        assert_eq!(registers[6], 4);
        #[cfg(feature = "profile")]
        {
            assert_eq!(profile.loop_compile_attempts(), 1);
            assert_eq!(profile.loop_compile_successes(), 1);
            assert_eq!(profile.loop_compile_failures(), 0);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn four_block_cycles_stay_counted_under_wider_bounded_discovery() {
        let code = [
            addi(5, 5, 1),
            beq(0, 0, 4),
            addi(6, 6, 1),
            beq(0, 0, 4),
            addi(7, 7, 1),
            beq(0, 0, 4),
            addi(8, 8, 1),
            beq(0, 0, -28),
        ];
        let machine = machine_with_code_at(&code, IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let blocks = [
            cached_id(&mut cache, &machine, IMAGE_START, &mut profile),
            cached_id(&mut cache, &machine, IMAGE_START + 8, &mut profile),
            cached_id(&mut cache, &machine, IMAGE_START + 16, &mut profile),
            cached_id(&mut cache, &machine, IMAGE_START + 24, &mut profile),
        ];
        for block in blocks {
            heat(&mut cache, block, usize::MAX, &mut profile);
        }
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        for index in 1..blocks.len() {
            for _ in 0..DOMINANT_EDGE_MINIMUM_OBSERVATIONS - 1 {
                assert!(observe_edge(
                    &mut cache,
                    blocks[index],
                    IMAGE_START + ((index + 1) % blocks.len()) as u32 * 8,
                    blocks[(index + 1) % blocks.len()],
                    &mut profile,
                ));
            }
        }
        for _ in 0..DOMINANT_EDGE_MINIMUM_OBSERVATIONS {
            assert!(observe_edge(
                &mut cache,
                blocks[0],
                IMAGE_START + 8,
                blocks[1],
                &mut profile,
            ));
        }
        assert_eq!(
            observe_and_compile_region(&mut cache, blocks[0], usize::MAX, &mut profile),
            0
        );
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        let region = cache.native_region_entry(blocks[0]).unwrap();
        assert_eq!(cache.native_loop_count(), 1);
        assert_eq!(region.entry.kind(), NativeEntryKind::Loop);
        assert_eq!(region.metadata.block_count(), 4);
        assert_eq!(region.entry.instruction_count(), code.len());
        assert_eq!(region.entry.minimum_instruction_count(), code.len());
        assert_eq!(region.entry.loop_unroll_factor(), 1);
        for copy in 0..4 {
            for (index, block) in blocks.into_iter().enumerate() {
                assert_eq!(
                    region
                        .metadata
                        .source_for_retired(copy * code.len() + (index + 1) * 2),
                    Some(block)
                );
            }
        }
        #[cfg(feature = "profile")]
        {
            assert_eq!(profile.region_selected_blocks(), 4);
            assert_eq!(profile.region_selected_instructions(), code.len() as u64);
            assert_eq!(profile.region_compiled_blocks(), 4);
            assert_eq!(profile.region_path_loop_closures(), 1);
            assert_eq!(profile.region_path_block_limit_stops(), 0);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn five_block_cycles_use_a_finite_region_without_widening_counted_loops() {
        let code = [
            addi(5, 5, 1),
            beq(0, 0, 4),
            addi(6, 6, 1),
            beq(0, 0, 4),
            addi(7, 7, 1),
            beq(0, 0, 4),
            addi(8, 8, 1),
            beq(0, 0, 4),
            addi(9, 9, 1),
            beq(0, 0, -36),
        ];
        let machine = machine_with_code_at(&code, IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let blocks = (0..5_u32)
            .map(|index| cached_id(&mut cache, &machine, IMAGE_START + index * 8, &mut profile))
            .collect::<Vec<_>>();
        for &block in &blocks {
            heat(&mut cache, block, usize::MAX, &mut profile);
        }
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );
        for index in 0..blocks.len() {
            let observations = if index == 0 {
                DOMINANT_EDGE_MINIMUM_OBSERVATIONS
            } else {
                DOMINANT_EDGE_MINIMUM_OBSERVATIONS - 1
            };
            for _ in 0..observations {
                let successor = blocks[(index + 1) % blocks.len()];
                let successor_pc = IMAGE_START + ((index + 1) % blocks.len()) as u32 * 8;
                assert!(observe_edge(
                    &mut cache,
                    blocks[index],
                    successor_pc,
                    successor,
                    &mut profile,
                ));
            }
        }
        assert_eq!(
            observe_and_compile_region(&mut cache, blocks[0], usize::MAX, &mut profile),
            0
        );
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        let region = cache.native_region_entry(blocks[0]).unwrap();
        assert_eq!(region.entry.kind(), NativeEntryKind::Bounded);
        assert_eq!(region.entry.instruction_count(), code.len());
        assert_eq!(region.metadata.block_count(), 5);
        assert_eq!(cache.native_loop_count(), 0);
        #[cfg(feature = "profile")]
        {
            assert_eq!(profile.loop_compile_attempts(), 1);
            assert_eq!(profile.loop_compile_failures(), 1);
            assert_eq!(profile.region_selected_blocks(), 5);
            assert_eq!(profile.region_compiled_blocks(), 5);
            assert_eq!(profile.region_path_loop_closures(), 1);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn dynamic_cycle_closures_fall_back_to_a_bounded_finite_region() {
        let jalr_x1 = 0x0000_8067;
        let code = [addi(5, 5, 1), beq(0, 0, 4), addi(6, 6, 1), jalr_x1];
        let machine = machine_with_code_at(&code, IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let first = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        let second = cached_id(&mut cache, &machine, IMAGE_START + 8, &mut profile);
        heat(&mut cache, first, usize::MAX, &mut profile);
        heat(&mut cache, second, usize::MAX, &mut profile);
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        for _ in 0..DOMINANT_EDGE_MINIMUM_OBSERVATIONS - 1 {
            assert!(observe_edge(
                &mut cache,
                second,
                IMAGE_START,
                first,
                &mut profile,
            ));
        }
        for _ in 0..DOMINANT_EDGE_MINIMUM_OBSERVATIONS {
            assert!(observe_edge(
                &mut cache,
                first,
                IMAGE_START + 8,
                second,
                &mut profile,
            ));
        }
        assert_eq!(
            observe_and_compile_region(&mut cache, first, usize::MAX, &mut profile),
            0
        );
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        let region = cache.native_region_entry(first).unwrap();
        assert_eq!(region.entry.kind(), NativeEntryKind::Bounded);
        assert_eq!(cache.native_loop_count(), 0);
        assert_eq!(region.metadata.block_count(), 2);
        assert_eq!(region.entry.instruction_count(), code.len());
        assert_eq!(region.entry.minimum_instruction_count(), code.len());
        assert_eq!(region.entry.loop_unroll_factor(), 1);
        assert_eq!(region.metadata.source_for_retired(2), Some(first));
        assert_eq!(region.metadata.source_for_retired(4), Some(second));
        #[cfg(feature = "profile")]
        {
            assert_eq!(profile.loop_compile_attempts(), 0);
            assert_eq!(profile.loop_compile_successes(), 0);
            assert_eq!(profile.loop_compile_failures(), 0);
            assert_eq!(profile.region_compile_successes(), 1);
            assert_eq!(profile.region_path_jalr_stops(), 1);
            assert_eq!(profile.region_selected_blocks(), 2);
            assert_eq!(profile.region_compiled_blocks(), 2);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn failed_loop_publication_preserves_the_native_basic_fallback() {
        let code = [addi(5, 5, 1), beq(0, 0, -4)];
        let machine = machine_with_code_at(&code, IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let head = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        heat(&mut cache, head, usize::MAX, &mut profile);
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        for _ in 0..DOMINANT_EDGE_MINIMUM_OBSERVATIONS {
            assert!(observe_edge(
                &mut cache,
                head,
                IMAGE_START,
                head,
                &mut profile,
            ));
        }
        assert_eq!(
            observe_and_compile_region(&mut cache, head, 0, &mut profile),
            0
        );
        assert_eq!(cache.staged_block_count(), 1);
        assert_eq!(cache.staged[0].compiled.instruction_count(), code.len());
        assert_eq!(
            cache.staged[0].compiled.minimum_instruction_count(),
            code.len()
        );
        assert_eq!(cache.staged[0].compiled.loop_unroll_factor(), 1);
        assert_eq!(flush_pending(&mut cache, 0, &mut profile), 0);

        assert!(matches!(
            cache.blocks[head.index()].basic_tier,
            BasicTier::Native(_)
        ));
        assert!(matches!(
            cache.blocks[head.index()].region_tier,
            RegionTier::Disabled
        ));
        assert!(cache.native_entry(head).is_some());
        assert!(cache.native_region_entry(head).is_none());
        assert_eq!(cache.native_block_count(), 1);
        assert_eq!(cache.native_region_count(), 0);
        assert_eq!(cache.native_loop_count(), 0);
        assert_eq!(cache.program_count(), 1);
        #[cfg(feature = "profile")]
        {
            assert_eq!(profile.loop_compile_attempts(), 1);
            assert_eq!(profile.loop_compile_successes(), 0);
            assert_eq!(profile.loop_compile_failures(), 1);
            assert_eq!(profile.region_compile_failures(), 1);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn invalid_deeper_continuations_fall_back_to_the_longest_valid_prefix() {
        let code = [
            beq(0, 0, 8),
            beq(0, 0, 0),
            beq(0, 0, -8),
            NOP,
            addi(7, 7, 1),
            beq(0, 0, 0),
        ];
        let machine = machine_with_code_at(&code, IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let first = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        let second = cached_id(&mut cache, &machine, IMAGE_START + 8, &mut profile);
        let invalid_third = cached_id(&mut cache, &machine, IMAGE_START + 16, &mut profile);
        for id in [first, second, invalid_third] {
            heat(&mut cache, id, usize::MAX, &mut profile);
        }
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        for _ in 1..DOMINANT_EDGE_MINIMUM_OBSERVATIONS {
            assert!(observe_edge(
                &mut cache,
                second,
                IMAGE_START + 16,
                invalid_third,
                &mut profile,
            ));
        }
        for _ in 0..DOMINANT_EDGE_MINIMUM_OBSERVATIONS {
            assert!(observe_edge(
                &mut cache,
                first,
                IMAGE_START + 8,
                second,
                &mut profile,
            ));
        }
        assert_eq!(
            observe_and_compile_region(&mut cache, first, usize::MAX, &mut profile),
            0
        );
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        let region = cache.native_region_entry(first).unwrap();
        assert_eq!(region.entry.kind(), NativeEntryKind::Bounded);
        assert_eq!(cache.native_loop_count(), 0);
        assert_eq!(region.metadata.block_count(), 2);
        assert_eq!(region.entry.instruction_count(), 2);
        assert_eq!(region.metadata.final_source(), second);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn failed_region_publication_preserves_basic_fallback() {
        let code = [beq(0, 0, 8), beq(0, 0, 0), addi(5, 5, 1), beq(0, 0, 0)];
        let machine = machine_with_code_at(&code, IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let head = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        let alternate = cached_id(&mut cache, &machine, IMAGE_START + 4, &mut profile);
        let successor = cached_id(&mut cache, &machine, IMAGE_START + 8, &mut profile);
        heat(&mut cache, head, usize::MAX, &mut profile);
        heat(&mut cache, successor, usize::MAX, &mut profile);
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );

        for _ in 0..7 {
            assert!(observe_edge(
                &mut cache,
                head,
                IMAGE_START + 8,
                successor,
                &mut profile,
            ));
        }
        assert!(observe_edge(
            &mut cache,
            head,
            IMAGE_START + 4,
            alternate,
            &mut profile,
        ));
        assert_eq!(
            observe_and_compile_region(&mut cache, head, 0, &mut profile),
            0
        );
        assert_eq!(cache.staged_block_count(), 1);
        assert_eq!(flush_pending(&mut cache, 0, &mut profile), 0);

        assert!(matches!(
            cache.blocks[head.index()].basic_tier,
            BasicTier::Native(_)
        ));
        assert!(matches!(
            cache.blocks[head.index()].region_tier,
            RegionTier::Disabled
        ));
        assert!(cache.native_entry(head).is_some());
        assert!(cache.native_region_entry(head).is_none());
        assert_eq!(cache.native_block_count(), 2);
        assert_eq!(cache.native_region_count(), 0);
        assert_eq!(cache.program_count(), 1);
        #[cfg(feature = "profile")]
        {
            assert_eq!(profile.region_compile_attempts(), 1);
            assert_eq!(profile.region_compile_successes(), 0);
            assert_eq!(profile.region_compile_failures(), 1);
        }
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    ))]
    #[test]
    fn clear_discards_staged_code_and_published_programs() {
        let machine = machine_with_code_at(&[addi(5, 0, 1), beq(0, 0, 0)], IMAGE_START);
        let mut cache = BlockCache::default();
        let mut profile = test_profile();
        let id = cached_id(&mut cache, &machine, IMAGE_START, &mut profile);
        heat(&mut cache, id, usize::MAX, &mut profile);
        assert_eq!(
            flush_pending(&mut cache, usize::MAX, &mut profile),
            PAGE_SIZE
        );
        assert_eq!(cache.program_count(), 1);

        let staged_machine = machine_with_code_at(&[addi(6, 0, 2), beq(0, 0, 0)], IMAGE_START + 8);
        let staged = cached_id(&mut cache, &staged_machine, IMAGE_START + 8, &mut profile);
        heat(&mut cache, staged, usize::MAX, &mut profile);
        assert_eq!(cache.staged_block_count(), 1);

        cache.clear();

        assert_eq!(cache.block_count(), 0);
        assert_eq!(cache.program_count(), 0);
        assert_eq!(cache.staged_block_count(), 0);
    }
}
