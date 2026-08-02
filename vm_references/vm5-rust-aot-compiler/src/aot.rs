//! Builds and owns the native blocks for one loaded ELF image.

use std::num::NonZeroU32;

use rv32vm_rust_common::{
    machine::Machine,
    memory::{ADDRESS_SPACE_SIZE, Image, PAGE_COUNT, PAGE_SHIFT, PAGE_SIZE},
};
use rv32vm_rust_x86_block_compiler::BlockInstruction;

use crate::linked::{LinkedBlock, LinkedEntry, LinkedProgram};
#[cfg(feature = "profile")]
use crate::profile::{GeneratedBlockProfile, LoadProfile};

const INSTRUCTIONS_PER_PAGE: usize = PAGE_SIZE / 4;
/// Longest native candidate formed by the eager compiler.
const MAX_NATIVE_INSTRUCTIONS: usize = 64;
/// Shortest native prefix worth retaining in the eager image.
const MIN_NATIVE_INSTRUCTIONS: usize = 2;
/// Largest executable input scanned during `LOAD`.
const MAX_SCANNED_INSTRUCTIONS: usize = 262_144;
/// Largest number of native blocks retained for one image.
const MAX_NATIVE_BLOCKS: usize = 8_192;
/// Largest total executable mapping size retained for one image (32 MiB).
const MAX_CODE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy)]
struct PreparationLimits {
    scanned_instructions: usize,
    native_blocks: usize,
    code_bytes: usize,
}

impl PreparationLimits {
    const PRODUCTION: Self = Self {
        scanned_instructions: MAX_SCANNED_INSTRUCTIONS,
        native_blocks: MAX_NATIVE_BLOCKS,
        code_bytes: MAX_CODE_BYTES,
    };

    #[cfg(test)]
    const fn new(scanned_instructions: usize, native_blocks: usize, code_bytes: usize) -> Self {
        Self {
            scanned_instructions,
            native_blocks,
            code_bytes,
        }
    }
}

#[derive(Clone, Copy)]
struct BlockId(NonZeroU32);

impl BlockId {
    fn new(index: usize) -> Self {
        let value = u32::try_from(index + 1).expect("block limit fits in u32");
        Self(NonZeroU32::new(value).expect("block IDs are one-based"))
    }

    const fn index(self) -> usize {
        (self.0.get() - 1) as usize
    }
}

#[derive(Clone, Copy, Default)]
enum Slot {
    #[default]
    Unseen,
    Unavailable,
    Native(BlockId),
}

struct BlockPage {
    entries: [Slot; INSTRUCTIONS_PER_PAGE],
}

impl BlockPage {
    fn new() -> Self {
        Self {
            entries: [Slot::Unseen; INSTRUCTIONS_PER_PAGE],
        }
    }
}

/// A bounded, image-scoped table of eagerly compiled native blocks.
pub(crate) struct NativeImage {
    pages: Box<[Option<Box<BlockPage>>]>,
    program: Option<LinkedProgram>,
    #[cfg(feature = "profile")]
    load_profile: LoadProfile,
    #[cfg(test)]
    staged_block_count: usize,
}

impl Default for NativeImage {
    fn default() -> Self {
        Self {
            pages: std::iter::repeat_with(|| None).take(PAGE_COUNT).collect(),
            program: None,
            #[cfg(feature = "profile")]
            load_profile: LoadProfile::default(),
            #[cfg(test)]
            staged_block_count: 0,
        }
    }
}

impl NativeImage {
    /// Eagerly compiles bounded native candidates from an immutable image.
    pub(crate) fn prepare(image: &Image) -> Self {
        Self::prepare_with_limits(image, PreparationLimits::PRODUCTION)
    }

    fn prepare_with_limits(image: &Image, limits: PreparationLimits) -> Self {
        debug_assert!(limits.scanned_instructions <= MAX_SCANNED_INSTRUCTIONS);
        debug_assert!(limits.native_blocks <= MAX_NATIVE_BLOCKS);
        debug_assert!(limits.code_bytes <= MAX_CODE_BYTES);

        let machine = Machine::new(image, &[], 0);
        let mut native = Self::default();
        let mut blocks = Vec::new();
        let mut code_bytes = 0;
        native.try_compile(&machine, image.entry, limits, &mut blocks, &mut code_bytes);

        let mut scanned = 0;
        'ranges: for range in &image.executable_file_ranges {
            let mut begins_block = true;
            let mut sequence_length = 0;
            let mut pc = range.start;

            while pc < range.end {
                if scanned == limits.scanned_instructions {
                    break 'ranges;
                }
                scanned += 1;
                if pc != range.start && pc.is_multiple_of(PAGE_SIZE as u32) {
                    begins_block = true;
                    sequence_length = 0;
                }

                if begins_block {
                    native.try_compile(&machine, pc, limits, &mut blocks, &mut code_bytes);
                }

                let Ok(instruction) = machine.fetch_decode(pc) else {
                    begins_block = true;
                    sequence_length = 0;
                    pc += 4;
                    continue;
                };
                if let Some(target) = instruction.direct_target() {
                    native.try_compile(&machine, target, limits, &mut blocks, &mut code_bytes);
                }

                if LinkedBlock::supports(instruction) && !LinkedBlock::ends_block(instruction) {
                    sequence_length += 1;
                    begins_block = sequence_length == MAX_NATIVE_INSTRUCTIONS;
                    if begins_block {
                        sequence_length = 0;
                    }
                } else {
                    begins_block = true;
                    sequence_length = 0;
                }
                pc += 4;
            }
        }

        #[cfg(test)]
        {
            native.staged_block_count = blocks.len();
        }
        native.program = LinkedProgram::publish(blocks, limits.code_bytes);
        #[cfg(feature = "profile")]
        {
            native.load_profile.code_bytes = code_bytes as u64;
            native.load_profile.mapped_bytes = native
                .program
                .as_ref()
                .map_or(0, |program| program.mapped_len() as u64);
        }
        native
    }

    pub(crate) fn get(&self, pc: u32) -> Option<LinkedEntry<'_>> {
        let Slot::Native(id) = self.slot(pc)? else {
            return None;
        };
        self.program.as_ref()?.entry(id.index())
    }

    fn try_compile(
        &mut self,
        machine: &Machine,
        pc: u32,
        limits: PreparationLimits,
        blocks: &mut Vec<LinkedBlock>,
        code_bytes: &mut usize,
    ) {
        if pc & 3 != 0
            || pc >= ADDRESS_SPACE_SIZE
            || blocks.len() == limits.native_blocks
            || !matches!(self.slot(pc), Some(Slot::Unseen))
        {
            return;
        }
        let Ok(instruction) = machine.fetch_decode(pc) else {
            return;
        };
        self.set_slot(pc, Slot::Unavailable);
        if !LinkedBlock::supports(instruction) {
            return;
        }

        let instructions = native_sequence(machine, pc);
        let Some(block) = LinkedBlock::compile(&instructions) else {
            return;
        };
        if block.instruction_count() < MIN_NATIVE_INSTRUCTIONS && !block.permits_singleton() {
            return;
        }
        let Some(next_code_bytes) = (*code_bytes).checked_add(block.code_len()) else {
            return;
        };
        if next_code_bytes > limits.code_bytes {
            return;
        }

        *code_bytes = next_code_bytes;
        let id = BlockId::new(blocks.len());
        #[cfg(feature = "profile")]
        {
            let profile = GeneratedBlockProfile::from_compiled(&block);
            self.load_profile.record_block(profile);
        }
        blocks.push(block);
        self.set_slot(pc, Slot::Native(id));
    }

    #[cfg(feature = "profile")]
    pub(crate) const fn load_profile(&self) -> LoadProfile {
        self.load_profile
    }

    fn slot(&self, pc: u32) -> Option<Slot> {
        if pc & 3 != 0 || pc >= ADDRESS_SPACE_SIZE {
            return None;
        }
        let page = (pc >> PAGE_SHIFT) as usize;
        let offset = (pc as usize & (PAGE_SIZE - 1)) / 4;
        Some(
            self.pages[page]
                .as_ref()
                .map_or(Slot::Unseen, |page| page.entries[offset]),
        )
    }

    fn set_slot(&mut self, pc: u32, value: Slot) {
        let page = (pc >> PAGE_SHIFT) as usize;
        let offset = (pc as usize & (PAGE_SIZE - 1)) / 4;
        let page = self.pages[page].get_or_insert_with(|| Box::new(BlockPage::new()));
        page.entries[offset] = value;
    }

    #[cfg(test)]
    pub(crate) fn attempted(&self, pc: u32) -> bool {
        !matches!(self.slot(pc), None | Some(Slot::Unseen))
    }

    #[cfg(test)]
    pub(crate) fn staged_block_count(&self) -> usize {
        self.staged_block_count
    }
}

fn native_sequence(machine: &Machine, start_pc: u32) -> Vec<BlockInstruction> {
    let mut instructions = Vec::with_capacity(MAX_NATIVE_INSTRUCTIONS);
    let mut pc = start_pc;

    loop {
        let instruction = machine.fetch_decode(pc);
        let ends_sequence = instruction.as_ref().map_or(true, |instruction| {
            !LinkedBlock::supports(*instruction) || LinkedBlock::ends_block(*instruction)
        });
        instructions.push(instruction);
        if ends_sequence || instructions.len() == MAX_NATIVE_INSTRUCTIONS {
            return instructions;
        }

        pc = pc.wrapping_add(4);
        if pc.is_multiple_of(PAGE_SIZE as u32) {
            return instructions;
        }
    }
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::{machine::Machine, memory::IMAGE_START};

    use super::{MAX_CODE_BYTES, NativeImage, PreparationLimits, native_sequence};
    use crate::linked::LinkedBlock;
    use crate::test_support::{addi, beq, image_with_code_at, lw};

    fn register(rd: u32, rs1: u32, rs2: u32, funct3: u32, funct7: u32) -> u32 {
        (funct7 << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x33
    }

    #[test]
    fn prepares_entry_and_sequences_after_precise_fallbacks() {
        let image = image_with_code_at(
            &[
                addi(5, 0, 1),
                addi(5, 5, 1),
                lw(6, 0, 0),
                addi(7, 0, 1),
                addi(7, 7, 1),
                0x0000_0073,
            ],
            IMAGE_START,
        );

        let native = NativeImage::prepare(&image);

        assert!(native.attempted(IMAGE_START));
        assert!(native.attempted(IMAGE_START + 12));
    }

    #[test]
    fn prepares_static_branch_targets() {
        let image = image_with_code_at(
            &[
                addi(5, 0, 1),
                beq(5, 0, 12),
                addi(6, 0, 1),
                addi(6, 6, 1),
                addi(7, 0, 1),
                addi(7, 7, 1),
            ],
            IMAGE_START,
        );

        let native = NativeImage::prepare(&image);

        assert!(native.attempted(IMAGE_START + 16));
    }

    #[test]
    fn prepares_every_rv32m_operation_as_one_native_sequence() {
        let code = (0..8)
            .map(|funct3| register(5, 6, 7, funct3, 1))
            .collect::<Vec<_>>();
        let image = image_with_code_at(&code, IMAGE_START);

        let native = NativeImage::prepare(&image);

        assert!(native.attempted(IMAGE_START));
        assert_eq!(native.staged_block_count(), 1);
        #[cfg(feature = "profile")]
        assert_eq!(native.load_profile().native_guest_instructions, 8);
    }

    #[test]
    fn retains_memory_singletons_and_discovers_their_successors() {
        let image = image_with_code_at(
            &[lw(5, 10, 0), lw(6, 10, 4), addi(7, 7, 1), addi(7, 7, 1)],
            IMAGE_START,
        );

        let native = NativeImage::prepare(&image);

        assert!(native.attempted(IMAGE_START));
        assert!(native.attempted(IMAGE_START + 4));
        assert!(native.attempted(IMAGE_START + 8));
        assert_eq!(native.staged_block_count(), 3);
        #[cfg(feature = "profile")]
        {
            assert_eq!(native.load_profile().native_guest_instructions, 4);
            assert_eq!(native.load_profile().fallthrough_blocks, 3);
        }
    }

    #[test]
    fn scans_only_file_backed_executable_words() {
        let image = image_with_code_at(&[0x0000_0073], IMAGE_START);

        let native = NativeImage::prepare(&image);

        assert!(native.attempted(IMAGE_START));
        assert!(!native.attempted(IMAGE_START + 4));
    }

    #[test]
    fn scan_limit_stops_at_the_exact_word() {
        let image = image_with_code_at(&[lw(5, 0, 0), lw(6, 0, 0), lw(7, 0, 0)], IMAGE_START);
        let limits = PreparationLimits::new(2, 8, MAX_CODE_BYTES);

        let native = NativeImage::prepare_with_limits(&image, limits);

        assert!(native.attempted(IMAGE_START));
        assert!(native.attempted(IMAGE_START + 4));
        assert!(!native.attempted(IMAGE_START + 8));
    }

    #[test]
    fn native_block_limit_stops_at_the_exact_block() {
        let image = image_with_code_at(
            &[
                addi(5, 5, 1),
                addi(5, 5, 1),
                lw(6, 0, 0),
                addi(7, 7, 1),
                addi(7, 7, 1),
                0x0000_0073,
            ],
            IMAGE_START,
        );

        let one =
            NativeImage::prepare_with_limits(&image, PreparationLimits::new(6, 1, MAX_CODE_BYTES));
        let two =
            NativeImage::prepare_with_limits(&image, PreparationLimits::new(6, 2, MAX_CODE_BYTES));

        assert_eq!(one.staged_block_count(), 1);
        assert!(!one.attempted(IMAGE_START + 12));
        assert_eq!(two.staged_block_count(), 2);
        assert!(two.attempted(IMAGE_START + 12));
    }

    #[test]
    fn emitted_code_limit_accepts_an_exact_fit() {
        let image = image_with_code_at(&[addi(5, 5, 1), addi(5, 5, 1), lw(6, 0, 0)], IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let code_len = LinkedBlock::compile(&native_sequence(&machine, IMAGE_START))
            .unwrap()
            .code_len();

        let exact =
            NativeImage::prepare_with_limits(&image, PreparationLimits::new(3, 1, code_len));
        let short =
            NativeImage::prepare_with_limits(&image, PreparationLimits::new(3, 1, code_len - 1));

        assert_eq!(exact.staged_block_count(), 1);
        assert_eq!(short.staged_block_count(), 0);
    }

    #[cfg(feature = "profile")]
    #[test]
    fn records_reusable_load_profile_for_generated_blocks() {
        let image = image_with_code_at(&[addi(5, 5, 1), beq(0, 0, -4)], IMAGE_START);

        let native = NativeImage::prepare(&image);
        let profile = native.load_profile();

        assert_eq!(profile.compiled_blocks, 1);
        assert_eq!(profile.native_guest_instructions, 2);
        assert!(profile.code_bytes > 0);
        assert_eq!(profile.branch_blocks, 1);
        assert_eq!(profile.fallthrough_blocks, 0);
        assert_eq!(profile.direct_jump_blocks, 0);
        #[cfg(all(
            target_arch = "x86_64",
            target_os = "linux",
            target_pointer_width = "64"
        ))]
        assert!(profile.mapped_bytes >= profile.code_bytes);
    }
}
