//! Owns native-code publication and invocation for x86-64 Linux.

use std::mem;

use rv32vm_rust_common::memory::NativeMemoryView;

use crate::{
    CompiledBlock, NativeEntryKind, NativeOutcome, SIDE_EXIT_FLAG,
    emitter::{OptimizationEvent, OptimizationEventKind},
    memory::ExecutableMemory,
};

type Entry = unsafe extern "C" fn(*mut u32, *const u8, *mut u8, u32) -> u64;

#[derive(Clone, Copy)]
struct EntryMetadata {
    offset: usize,
    instruction_count: usize,
    minimum_instruction_count: usize,
    loop_unroll_factor: usize,
    kind: NativeEntryKind,
    optimization_event_start: usize,
    optimization_event_end: usize,
}

/// Owns one executable mapping containing one or more native blocks.
pub struct NativeProgram {
    memory: ExecutableMemory,
    entries: Vec<EntryMetadata>,
    optimization_events: Vec<OptimizationEvent>,
}

impl NativeProgram {
    pub fn publish(blocks: Vec<CompiledBlock>, code_budget: usize) -> Option<Self> {
        let code_len = blocks.iter().try_fold(0_usize, |length, block| {
            length.checked_add(block.code_len())
        })?;
        if code_len == 0 {
            return None;
        }

        let mut code = Vec::with_capacity(code_len);
        let mut entries = Vec::with_capacity(blocks.len());
        let mut optimization_events = Vec::new();
        for block in blocks {
            let optimization_event_start = optimization_events.len();
            optimization_events.extend_from_slice(&block.optimization_events);
            entries.push(EntryMetadata {
                offset: code.len(),
                instruction_count: block.instruction_count(),
                minimum_instruction_count: block.minimum_instruction_count(),
                loop_unroll_factor: block.loop_unroll_factor(),
                kind: block.kind(),
                optimization_event_start,
                optimization_event_end: optimization_events.len(),
            });
            code.extend_from_slice(&block.code);
        }

        let memory = ExecutableMemory::publish(&code, code_budget)?;
        Some(Self {
            memory,
            entries,
            optimization_events,
        })
    }

    pub fn entry(&self, index: usize) -> Option<NativeEntry<'_>> {
        Some(NativeEntry {
            program: self,
            metadata: *self.entries.get(index)?,
        })
    }

    pub const fn mapped_len(&self) -> usize {
        self.memory.len()
    }
}

/// A native block entry tied to the executable program that owns it.
#[derive(Clone, Copy)]
pub struct NativeEntry<'a> {
    program: &'a NativeProgram,
    metadata: EntryMetadata,
}

impl NativeEntry<'_> {
    /// Maximum bounded-path count, or the instruction count of one loop cycle.
    pub const fn instruction_count(&self) -> usize {
        self.metadata.instruction_count
    }

    /// Smallest instruction budget that can enter this native code.
    pub const fn minimum_instruction_count(&self) -> usize {
        self.metadata.minimum_instruction_count
    }

    /// Number of logical guest cycles in one counted host-loop iteration.
    pub const fn loop_unroll_factor(&self) -> usize {
        self.metadata.loop_unroll_factor
    }

    pub const fn kind(&self) -> NativeEntryKind {
        self.metadata.kind
    }

    /// Returns the complementary-shift fusions and dead shift writes that
    /// actually occurred in a committed native prefix. Loop outcomes may span
    /// multiple logical cycles; bounded guard exits count only earlier events.
    pub fn optimization_counts(&self, retired: usize) -> (usize, usize) {
        let instruction_count = self.instruction_count();
        if instruction_count == 0 {
            return (0, 0);
        }
        let events = &self.program.optimization_events
            [self.metadata.optimization_event_start..self.metadata.optimization_event_end];
        let complete_cycles = retired / instruction_count;
        let remainder = retired % instruction_count;
        let mut fused_rotates = 0_usize;
        let mut elided_shifts = 0_usize;
        for event in events {
            let count = complete_cycles + usize::from(event.retired_offset < remainder);
            match event.kind {
                OptimizationEventKind::FusedRotate => fused_rotates += count,
                OptimizationEventKind::ElidedShift => elided_shifts += count,
            }
        }
        (fused_rotates, elided_shifts)
    }

    /// Executes the native block against the supplied current-run memory view.
    pub fn execute(
        &self,
        registers: &mut [u32; 32],
        memory: NativeMemoryView<'_>,
    ) -> NativeOutcome {
        assert_eq!(
            self.kind(),
            NativeEntryKind::Bounded,
            "counted loop entries require execute_with_limit"
        );
        self.execute_raw(registers, memory, 0)
    }

    /// Executes this entry only when `remaining` permits its complete bounded
    /// path or at least one complete counted-loop group.
    ///
    /// Loop invocations are capped so the exact retired count never collides
    /// with the outcome's side-exit bit. Returning `None` performs no native
    /// mutation.
    pub fn execute_with_limit(
        &self,
        registers: &mut [u32; 32],
        memory: NativeMemoryView<'_>,
        remaining: u64,
    ) -> Option<NativeOutcome> {
        let iteration_budget = match self.kind() {
            NativeEntryKind::Bounded => {
                if remaining < self.minimum_instruction_count() as u64 {
                    return None;
                }
                0
            }
            NativeEntryKind::Loop => {
                loop_iteration_budget(self.minimum_instruction_count(), remaining)?
            }
        };
        Some(self.execute_raw(registers, memory, iteration_budget))
    }

    fn execute_raw(
        &self,
        registers: &mut [u32; 32],
        memory: NativeMemoryView<'_>,
        iteration_budget: u32,
    ) -> NativeOutcome {
        debug_assert!(self.metadata.offset < self.program.memory.len());
        // SAFETY: Every offset was recorded at the start of a finalized block
        // while assembling this still-live executable program.
        let address = unsafe { self.program.memory.address().add(self.metadata.offset) };
        debug_assert_eq!(size_of::<Entry>(), size_of::<*const u8>());
        // SAFETY: `address` names finalized bytes emitted for `Entry`.
        let entry = unsafe { mem::transmute::<*const u8, Entry>(address) };
        // SAFETY: The entry follows the private ABI, its RX mapping is alive,
        // and `registers` is exclusively borrowed for the synchronous call.
        NativeOutcome::from_raw(unsafe {
            entry(
                registers.as_mut_ptr(),
                memory.permissions(),
                memory.data(),
                iteration_budget,
            )
        })
    }
}

fn loop_iteration_budget(minimum_instruction_count: usize, remaining: u64) -> Option<u32> {
    let quantum = u64::try_from(minimum_instruction_count).ok()?;
    if quantum == 0 {
        return None;
    }
    let maximum_iterations = u64::from(SIDE_EXIT_FLAG - 1) / quantum;
    let iterations = (remaining / quantum).min(maximum_iterations);
    u32::try_from(iterations)
        .ok()
        .filter(|&iterations| iterations != 0)
}

/// Owns one executable native block and its guest-instruction count.
pub struct NativeBlock {
    program: NativeProgram,
}

impl NativeBlock {
    pub fn publish(block: CompiledBlock, code_budget: usize) -> Option<Self> {
        let program = NativeProgram::publish(vec![block], code_budget)?;
        Some(Self { program })
    }

    pub const fn mapped_len(&self) -> usize {
        self.program.mapped_len()
    }

    pub fn instruction_count(&self) -> usize {
        self.program
            .entry(0)
            .expect("single-block program has one entry")
            .instruction_count()
    }

    pub fn minimum_instruction_count(&self) -> usize {
        self.program
            .entry(0)
            .expect("single-block program has one entry")
            .minimum_instruction_count()
    }

    pub fn loop_unroll_factor(&self) -> usize {
        self.program
            .entry(0)
            .expect("single-block program has one entry")
            .loop_unroll_factor()
    }

    pub fn kind(&self) -> NativeEntryKind {
        self.program
            .entry(0)
            .expect("single-block program has one entry")
            .kind()
    }

    pub fn execute(
        &self,
        registers: &mut [u32; 32],
        memory: NativeMemoryView<'_>,
    ) -> NativeOutcome {
        self.program
            .entry(0)
            .expect("single-block program has one entry")
            .execute(registers, memory)
    }

    pub fn execute_with_limit(
        &self,
        registers: &mut [u32; 32],
        memory: NativeMemoryView<'_>,
        remaining: u64,
    ) -> Option<NativeOutcome> {
        self.program
            .entry(0)
            .expect("single-block program has one entry")
            .execute_with_limit(registers, memory, remaining)
    }
}

#[cfg(test)]
mod tests {
    core::arch::global_asm!(
        r#"
        .text
        .globl rv32vm_test_call_loop_with_callee_sentinels
        .type rv32vm_test_call_loop_with_callee_sentinels,@function
rv32vm_test_call_loop_with_callee_sentinels:
        pushq %r12
        pushq %r13
        pushq %r14
        pushq %r15
        subq $8, %rsp
        movq %r9, %r12
        movq %rdi, %rax
        movq %rsi, %rdi
        movq %rdx, %rsi
        movq %rcx, %rdx
        movl %r8d, %ecx
        movabsq $0x13579bdf2468ace0, %r13
        movabsq $0x0fedcba987654321, %r14
        movabsq $0x55aa33cc77ee11dd, %r15
        call *%rax
        movq %r13, 0(%r12)
        movq %r14, 8(%r12)
        movq %r15, 16(%r12)
        addq $8, %rsp
        popq %r15
        popq %r14
        popq %r13
        popq %r12
        retq
        .size rv32vm_test_call_loop_with_callee_sentinels, .-rv32vm_test_call_loop_with_callee_sentinels
        .pushsection .note.GNU-stack,"",@progbits
        .popsection
        "#,
        options(att_syntax)
    );

    unsafe extern "C" {
        fn rv32vm_test_call_loop_with_callee_sentinels(
            entry: *const u8,
            registers: *mut u32,
            permissions: *const u8,
            data: *mut u8,
            iterations: u32,
            observed: *mut u64,
        ) -> u64;
    }

    const CALLEE_SENTINELS: [u64; 3] = [
        0x1357_9bdf_2468_ace0,
        0x0fed_cba9_8765_4321,
        0x55aa_33cc_77ee_11dd,
    ];

    use rv32vm_rust_common::{
        GuestTrap,
        machine::{Machine, Termination},
        memory::{ADDRESS_SPACE_SIZE, IMAGE_START, PAGE_SIZE, STACK_END, STACK_START},
    };

    use super::{NativeBlock, NativeProgram, loop_iteration_budget};
    use crate::test_support::{NOP, addi, decoded_block, machine_with_code};
    use crate::{
        BlockInstruction, CompiledBlock, NativeEntryKind, NativeOutcome, RegionBlock,
        SIDE_EXIT_FLAG,
    };

    fn branch(funct3: u32, rs1: u32, rs2: u32, offset: i32) -> u32 {
        let immediate = offset as u32 & 0x1fff;
        ((immediate >> 12) << 31)
            | (((immediate >> 5) & 0x3f) << 25)
            | (rs2 << 20)
            | (rs1 << 15)
            | (funct3 << 12)
            | (((immediate >> 1) & 0xf) << 8)
            | (((immediate >> 11) & 1) << 7)
            | 0x63
    }

    fn jal(rd: u32, offset: i32) -> u32 {
        let immediate = offset as u32 & 0x1f_ffff;
        ((immediate >> 20) << 31)
            | (((immediate >> 1) & 0x3ff) << 21)
            | (((immediate >> 11) & 1) << 20)
            | (((immediate >> 12) & 0xff) << 12)
            | (rd << 7)
            | 0x6f
    }

    fn upper_immediate(opcode: u32, rd: u32, value: u32) -> u32 {
        (value & 0xffff_f000) | (rd << 7) | opcode
    }

    fn immediate(rd: u32, rs1: u32, funct3: u32, immediate: u32) -> u32 {
        ((immediate & 0xfff) << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x13
    }

    fn register(rd: u32, rs1: u32, rs2: u32, funct3: u32, funct7: u32) -> u32 {
        (funct7 << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x33
    }

    fn load(rd: u32, rs1: u32, funct3: u32, immediate: i32) -> u32 {
        ((immediate as u32 & 0xfff) << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x03
    }

    fn store(rs2: u32, rs1: u32, funct3: u32, immediate: i32) -> u32 {
        let immediate = immediate as u32 & 0xfff;
        ((immediate >> 5) << 25)
            | (rs2 << 20)
            | (rs1 << 15)
            | (funct3 << 12)
            | ((immediate & 0x1f) << 7)
            | 0x23
    }

    fn jalr(rd: u32, rs1: u32, immediate: i32) -> u32 {
        ((immediate as u32 & 0xfff) << 20) | (rs1 << 15) | (rd << 7) | 0x67
    }

    fn native_block(code: &[u32]) -> NativeBlock {
        let machine = machine_with_code(code, IMAGE_START);
        let block = decoded_block(&machine, IMAGE_START);
        let compiled = CompiledBlock::compile(&block).unwrap();
        NativeBlock::publish(compiled, usize::MAX).unwrap()
    }

    fn decoded(machine: &Machine, start: u32, count: usize) -> Vec<BlockInstruction> {
        (0..count)
            .map(|index| machine.fetch_decode(start + index as u32 * 4))
            .collect()
    }

    fn native_region(machine: &Machine, blocks: &[(u32, usize)]) -> NativeBlock {
        let decoded = blocks
            .iter()
            .map(|&(start, count)| decoded(machine, start, count))
            .collect::<Vec<_>>();
        let region = decoded
            .iter()
            .map(|instructions| RegionBlock::new(instructions))
            .collect::<Vec<_>>();
        let compiled = CompiledBlock::compile_region(&region).unwrap();
        NativeBlock::publish(compiled, usize::MAX).unwrap()
    }

    fn native_unrolled_region(machine: &Machine, blocks: &[(u32, usize)]) -> NativeBlock {
        let decoded = blocks
            .iter()
            .map(|&(start, count)| decoded(machine, start, count))
            .collect::<Vec<_>>();
        let region = decoded
            .iter()
            .map(|instructions| RegionBlock::new(instructions))
            .collect::<Vec<_>>();
        let compiled = CompiledBlock::compile_unrolled_region(&region).unwrap();
        NativeBlock::publish(compiled, usize::MAX).unwrap()
    }

    fn native_loop(machine: &Machine, blocks: &[(u32, usize)]) -> NativeBlock {
        let decoded = blocks
            .iter()
            .map(|&(start, count)| decoded(machine, start, count))
            .collect::<Vec<_>>();
        let region = decoded
            .iter()
            .map(|instructions| RegionBlock::new(instructions))
            .collect::<Vec<_>>();
        let compiled = CompiledBlock::compile_loop(&region).unwrap();
        NativeBlock::publish(compiled, usize::MAX).unwrap()
    }

    fn native_grouped_loop(
        machine: &Machine,
        blocks: &[(u32, usize)],
        group_factor: usize,
    ) -> NativeBlock {
        let decoded = blocks
            .iter()
            .map(|&(start, count)| decoded(machine, start, count))
            .collect::<Vec<_>>();
        let region = decoded
            .iter()
            .map(|instructions| RegionBlock::new(instructions))
            .collect::<Vec<_>>();
        let compiled = CompiledBlock::compile_grouped_loop(&region, group_factor).unwrap();
        NativeBlock::publish(compiled, usize::MAX).unwrap()
    }

    fn six_hot_register_updates() -> Vec<u32> {
        let mut code = Vec::new();
        for register in 5..=10 {
            code.push(addi(register, register, 1));
            code.push(addi(register, register, 1));
        }
        code
    }

    fn execute_native(native: &NativeBlock, machine: &mut Machine) -> NativeOutcome {
        let memory = machine.memory.native_view();
        native.execute(&mut machine.registers, memory)
    }

    fn interpret_side_exit(machine: &mut Machine, outcome: NativeOutcome) -> GuestTrap {
        assert!(outcome.needs_interpreter());
        machine.pc = outcome.next_pc();
        machine.retired += u64::from(outcome.retired());
        let retired_before = machine.retired;
        let instruction = machine.fetch_decode(machine.pc);
        let Some(Termination::Trap(trap)) = machine.execute_one(instruction) else {
            panic!("native memory side exit must reproduce an interpreter trap");
        };
        assert_eq!(machine.retired, retired_before);
        trap
    }

    fn execute_loop(
        native: &NativeBlock,
        machine: &mut Machine,
        remaining: u64,
    ) -> Option<NativeOutcome> {
        let memory = machine.memory.native_view();
        native.execute_with_limit(&mut machine.registers, memory, remaining)
    }

    fn execute_loop_with_callee_sentinels(
        native: &NativeBlock,
        machine: &mut Machine,
        remaining: u64,
    ) -> Option<(NativeOutcome, [u64; 3])> {
        assert_eq!(native.kind(), NativeEntryKind::Loop);
        let iterations = loop_iteration_budget(native.minimum_instruction_count(), remaining)?;
        let entry = native
            .program
            .entry(0)
            .expect("single-block program has one entry");
        // SAFETY: The offset was recorded at the start of this finalized loop
        // entry and the executable mapping remains owned by `native`.
        let address = unsafe { entry.program.memory.address().add(entry.metadata.offset) };
        let memory = machine.memory.native_view();
        let mut observed = [0_u64; 3];
        // SAFETY: The assembly trampoline follows both the System V ABI and
        // the private generated-entry ABI. It synchronously borrows all
        // pointers and records the callee-saved values after the call.
        let raw = unsafe {
            rv32vm_test_call_loop_with_callee_sentinels(
                address,
                machine.registers.as_mut_ptr(),
                memory.permissions(),
                memory.data(),
                iterations,
                observed.as_mut_ptr(),
            )
        };
        Some((NativeOutcome::from_raw(raw), observed))
    }

    fn assert_matches_interpreter(code: &[u32], registers: &[(usize, u32)]) {
        let machine = machine_with_code(code, IMAGE_START);
        let block = decoded_block(&machine, IMAGE_START);
        let compiled = CompiledBlock::compile(&block).unwrap();
        let native = NativeBlock::publish(compiled, usize::MAX).unwrap();
        let mut expected = machine_with_code(code, IMAGE_START);
        let mut actual = machine_with_code(code, IMAGE_START);
        for &(register, value) in registers {
            expected.registers[register] = value;
            actual.registers[register] = value;
        }

        for _ in 0..native.instruction_count() {
            let instruction = expected.fetch_decode(expected.pc);
            assert!(expected.execute_one(instruction).is_none());
        }
        let memory = actual.memory.native_view();
        let outcome = native.execute(&mut actual.registers, memory);
        actual.pc = outcome.next_pc();

        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired() as usize, native.instruction_count());
        assert_eq!(actual.registers, expected.registers);
        assert_eq!(actual.pc, expected.pc);
    }

    #[test]
    fn executes_upper_immediates_jumps_and_fence() {
        assert_matches_interpreter(&[upper_immediate(0x37, 5, 0x8123_4000), NOP], &[]);
        assert_matches_interpreter(&[upper_immediate(0x17, 5, 0xffff_f000), NOP], &[]);
        assert_matches_interpreter(&[0x0000_000f, NOP], &[]);
        assert_matches_interpreter(&[NOP, jal(5, 8)], &[]);
    }

    #[test]
    fn executes_immediate_operations() {
        let cases = [
            (addi(5, 6, -1), 0),
            (immediate(5, 6, 2, 0xfff), 0x8000_0000),
            (immediate(5, 6, 3, 0xfff), 0xffff_fffe),
            (immediate(5, 6, 4, 0x55a), 0xaa55_aa55),
            (immediate(5, 6, 6, 0x055), 0xaa00_aa00),
            (immediate(5, 6, 7, 0x0ff), 0xaa55_aa55),
            (immediate(5, 6, 1, 31), 1),
            (immediate(5, 6, 5, 31), 0x8000_0000),
            (immediate(5, 6, 5, (0x20 << 5) | 31), 0x8000_0000),
        ];

        for (instruction, source) in cases {
            assert_matches_interpreter(&[instruction, NOP], &[(6, source)]);
        }
    }

    #[test]
    fn fused_complementary_shifts_match_the_interpreter_for_aliases_and_counts() {
        let values = [0, 1, u32::MAX, i32::MIN as u32, 0x8765_4321, 0x55aa_33cc];
        for left_count in 1..32 {
            let right_count = 32 - left_count;
            for destination in [5, 6, 7, 8] {
                let code = [
                    immediate(6, 5, 1, left_count),
                    addi(9, 9, 1),
                    immediate(7, 5, 5, right_count),
                    register(destination, 7, 6, 6, 0),
                    addi(6, 0, 11),
                    addi(7, 0, 12),
                    NOP,
                ];
                for value in values {
                    assert_matches_interpreter(&code, &[(5, value), (9, 41)]);
                }
            }
        }

        let zero_source = [
            immediate(6, 0, 1, 8),
            immediate(7, 0, 5, 24),
            register(8, 6, 7, 6, 0),
            addi(6, 0, 11),
            addi(7, 0, 12),
            NOP,
        ];
        assert_matches_interpreter(&zero_source, &[(8, u32::MAX)]);
    }

    #[test]
    fn optimization_events_follow_exact_retired_prefixes_and_loop_quanta() {
        let bounded_code = [
            immediate(6, 5, 1, 8),
            addi(9, 9, 1),
            immediate(7, 5, 5, 24),
            register(8, 6, 7, 6, 0),
            addi(6, 0, 11),
            addi(7, 0, 12),
        ];
        let bounded = native_block(&bounded_code);
        let entry = bounded.program.entry(0).unwrap();
        assert_eq!(entry.optimization_counts(0), (0, 0));
        assert_eq!(entry.optimization_counts(1), (0, 1));
        assert_eq!(entry.optimization_counts(3), (0, 2));
        assert_eq!(entry.optimization_counts(4), (1, 2));
        assert_eq!(entry.optimization_counts(bounded_code.len()), (1, 2));

        let loop_code = [
            immediate(6, 5, 1, 8),
            immediate(7, 5, 5, 24),
            register(8, 6, 7, 6, 0),
            addi(6, 0, 11),
            addi(7, 0, 12),
            jal(0, -20),
        ];
        let mut machine = machine_with_code(&loop_code, IMAGE_START);
        machine.registers[5] = 0x8765_4321;
        let grouped = native_grouped_loop(&machine, &[(IMAGE_START, loop_code.len())], 4);
        let outcome = execute_loop(&grouped, &mut machine, 24).unwrap();
        assert_eq!(grouped.instruction_count(), loop_code.len());
        assert_eq!(grouped.minimum_instruction_count(), 24);
        assert_eq!(outcome.retired(), 24);
        assert_eq!(
            grouped.program.entry(0).unwrap().optimization_counts(24),
            (4, 8)
        );
        assert_eq!(machine.registers[8], 0x8765_4321_u32.rotate_left(8));
    }

    #[test]
    fn elided_shifts_still_count_before_each_branch_outcome() {
        let code = [
            immediate(6, 5, 1, 8),
            immediate(7, 5, 5, 24),
            register(8, 6, 7, 6, 0),
            addi(6, 0, 11),
            addi(7, 0, 12),
            branch(0, 10, 11, 8),
        ];
        let native = native_block(&code);
        for (left, right, next_pc) in [(1, 2, IMAGE_START + 24), (3, 3, IMAGE_START + 28)] {
            let mut machine = machine_with_code(&code, IMAGE_START);
            machine.registers[5] = 0x8765_4321;
            machine.registers[10] = left;
            machine.registers[11] = right;
            let outcome = execute_native(&native, &mut machine);
            assert_eq!(outcome.retired(), code.len() as u32);
            assert_eq!(outcome.next_pc(), next_pc);
            assert_eq!(machine.registers[8], 0x8765_4321_u32.rotate_left(8));
            assert_eq!(
                native
                    .program
                    .entry(0)
                    .unwrap()
                    .optimization_counts(code.len()),
                (1, 2)
            );
        }
    }

    #[test]
    fn region_guard_before_a_fused_span_counts_no_later_events() {
        let code = [
            branch(0, 10, 11, 28),
            immediate(6, 5, 1, 8),
            immediate(7, 5, 5, 24),
            register(8, 6, 7, 6, 0),
            addi(6, 0, 11),
            addi(7, 0, 12),
            NOP,
        ];
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_region(&template, &[(IMAGE_START, 1), (IMAGE_START + 4, 6)]);
        let entry = native.program.entry(0).unwrap();

        let mut guarded = machine_with_code(&code, IMAGE_START);
        guarded.registers[10] = 3;
        guarded.registers[11] = 3;
        let outcome = execute_native(&native, &mut guarded);
        assert_eq!(outcome.retired(), 1);
        assert_eq!(
            entry.optimization_counts(outcome.retired() as usize),
            (0, 0)
        );
        assert_eq!(guarded.registers[8], 0);

        let mut completed = machine_with_code(&code, IMAGE_START);
        completed.registers[5] = 0x8765_4321;
        completed.registers[10] = 3;
        completed.registers[11] = 4;
        let outcome = execute_native(&native, &mut completed);
        assert_eq!(outcome.retired(), code.len() as u32);
        assert_eq!(
            entry.optimization_counts(outcome.retired() as usize),
            (1, 2)
        );
        assert_eq!(completed.registers[8], 0x8765_4321_u32.rotate_left(8));
    }

    #[test]
    fn compact_cached_immediates_match_the_interpreter_at_encoding_boundaries() {
        let code = [
            addi(5, 5, 0),
            addi(5, 5, 0),
            addi(5, 5, -129),
            addi(5, 5, -128),
            addi(5, 5, 127),
            addi(5, 5, 128),
            immediate(5, 5, 4, 0x07f),
            immediate(5, 5, 6, 0xf80),
            immediate(5, 5, 7, 0x055),
            NOP,
        ];
        assert_matches_interpreter(&code, &[(5, 0x8765_4321)]);
    }

    #[test]
    fn executes_register_operations() {
        let cases = [
            (register(5, 6, 7, 0, 0), u32::MAX, 1),
            (register(5, 6, 7, 0, 0x20), 0, 1),
            (register(5, 6, 7, 1, 0), 1, 33),
            (register(5, 6, 7, 2, 0), u32::MAX, 0),
            (register(5, 6, 7, 3, 0), 0, 1),
            (register(5, 6, 7, 4, 0), 0xaa55_aa55, 0x0f0f_0f0f),
            (register(5, 6, 7, 5, 0), 0x8000_0000, 31),
            (register(5, 6, 7, 5, 0x20), 0x8000_0000, 31),
            (register(5, 6, 7, 6, 0), 0xaa00_aa00, 0x0055_0055),
            (register(5, 6, 7, 7, 0), 0xaa55_aa55, 0x0f0f_0f0f),
            (register(5, 6, 7, 0, 1), u32::MAX, 2),
            (register(5, 6, 7, 1, 1), 0x8000_0000, 2),
            (register(5, 6, 7, 2, 1), 0x8000_0000, u32::MAX),
            (register(5, 6, 7, 3, 1), u32::MAX, u32::MAX),
            (register(5, 6, 7, 4, 1), (-7_i32) as u32, 3),
            (register(5, 6, 7, 5, 1), u32::MAX, 3),
            (register(5, 6, 7, 6, 1), (-7_i32) as u32, 3),
            (register(5, 6, 7, 7, 1), u32::MAX, 3),
        ];

        for (instruction, left, right) in cases {
            assert_matches_interpreter(&[instruction, NOP], &[(6, left), (7, right)]);
        }
    }

    #[test]
    fn cached_register_aliases_match_the_interpreter() {
        let code = [
            addi(5, 5, 3),
            addi(5, 5, -2),
            register(5, 5, 6, 0, 0),
            register(5, 7, 5, 0, 0),
            register(5, 5, 6, 0, 0x20),
            register(5, 5, 6, 1, 0),
            register(5, 5, 6, 5, 0),
            register(5, 5, 6, 5, 0x20),
            register(5, 5, 6, 4, 0),
            register(5, 5, 6, 6, 0),
            register(5, 5, 6, 7, 0),
            register(5, 5, 6, 0, 1),
            register(5, 6, 5, 0, 0x20),
            register(5, 6, 5, 0, 1),
            NOP,
        ];

        assert_matches_interpreter(&code, &[(5, 0x8765_4321), (6, 5), (7, 0x1020_3040)]);

        let mulhsu = [
            addi(5, 5, 1),
            addi(5, 5, 2),
            addi(6, 6, 3),
            addi(6, 6, 4),
            register(7, 5, 6, 2, 1),
            addi(5, 5, 5),
            addi(6, 6, 6),
            NOP,
        ];
        assert_matches_interpreter(&mulhsu, &[(5, 0x8765_4321), (6, 0x1020_3040)]);

        let three_slots = [
            addi(5, 5, 1),
            addi(5, 5, 2),
            addi(6, 6, 3),
            addi(6, 6, 4),
            addi(7, 7, 5),
            addi(7, 7, 6),
            register(5, 5, 6, 0, 0),
            register(6, 6, 7, 4, 0),
            register(7, 7, 5, 6, 0),
            NOP,
        ];
        assert_matches_interpreter(&three_slots, &[(5, 10), (6, 20), (7, 30)]);
    }

    #[test]
    fn direct_simple_alu_operand_locations_match_the_interpreter() {
        let operations = [(0, 0), (0, 0x20), (4, 0), (6, 0), (7, 0), (0, 1)];
        for (funct3, funct7) in operations {
            let hot_three = [
                addi(5, 5, 0),
                addi(5, 5, 0),
                addi(6, 6, 0),
                addi(6, 6, 0),
                addi(7, 7, 0),
                addi(7, 7, 0),
            ];
            let registers = [(5, 0x8765_4321), (6, 0x1020_3040), (7, 0x0506_0708)];

            let mut code = hot_three.to_vec();
            code.extend_from_slice(&[register(5, 6, 7, funct3, funct7), NOP]);
            assert_matches_interpreter(&code, &registers);

            let mut code = hot_three[..4].to_vec();
            code.extend_from_slice(&[register(8, 5, 6, funct3, funct7), NOP]);
            assert_matches_interpreter(&code, &registers);

            let mut code = hot_three[..2].to_vec();
            code.extend_from_slice(&[register(5, 5, 8, funct3, funct7), NOP]);
            assert_matches_interpreter(&code, &[(5, 0x8765_4321), (8, 0x1020_3040)]);

            let mut code = hot_three[..2].to_vec();
            code.extend_from_slice(&[register(5, 5, 0, funct3, funct7), NOP]);
            assert_matches_interpreter(&code, &[(5, 0x8765_4321)]);

            let mut code = hot_three[..2].to_vec();
            code.extend_from_slice(&[register(5, 0, 8, funct3, funct7), NOP]);
            assert_matches_interpreter(&code, &[(5, 0x8765_4321), (8, 0x1020_3040)]);

            if funct7 != 0x20 {
                let mut code = hot_three[..4].to_vec();
                code.extend_from_slice(&[register(5, 6, 5, funct3, funct7), NOP]);
                assert_matches_interpreter(&code, &registers);
            }
        }
    }

    #[test]
    fn cached_dirty_values_spill_on_both_branch_edges() {
        let code = [addi(5, 5, 1), addi(5, 5, 2), branch(0, 6, 7, 8)];

        assert_matches_interpreter(&code, &[(5, 10), (6, 7), (7, 7)]);
        assert_matches_interpreter(&code, &[(5, 10), (6, 7), (7, 8)]);
    }

    #[test]
    fn predicted_region_branches_preserve_exact_hot_and_guard_exits() {
        let cases = [
            (0, (5, 5), (5, 6)),
            (1, (5, 6), (5, 5)),
            (4, (u32::MAX, 0), (0, u32::MAX)),
            (5, (0, u32::MAX), (u32::MAX, 0)),
            (6, (0, 1), (1, 0)),
            (7, (1, 0), (0, 1)),
        ];

        for (funct3, taken, not_taken) in cases {
            let code = [
                addi(5, 5, 1),
                branch(funct3, 6, 7, 12),
                addi(5, 5, 2),
                NOP,
                addi(5, 5, 4),
                NOP,
            ];
            let template = machine_with_code(&code, IMAGE_START);
            let predicted_taken = native_region(
                &template,
                &[(IMAGE_START, 2), (IMAGE_START.wrapping_add(16), 1)],
            );
            let predicted_fallthrough = native_region(
                &template,
                &[(IMAGE_START, 2), (IMAGE_START.wrapping_add(8), 1)],
            );

            let mut actual = machine_with_code(&code, IMAGE_START);
            actual.registers[5] = 10;
            actual.registers[6] = taken.0;
            actual.registers[7] = taken.1;
            let outcome = execute_native(&predicted_taken, &mut actual);
            assert!(!outcome.needs_interpreter());
            assert_eq!(outcome.retired(), 3);
            assert_eq!(
                outcome.retired() as usize,
                predicted_taken.instruction_count()
            );
            assert_eq!(outcome.next_pc(), IMAGE_START + 20);
            assert_eq!(actual.registers[5], 15);

            let mut actual = machine_with_code(&code, IMAGE_START);
            actual.registers[5] = 10;
            actual.registers[6] = not_taken.0;
            actual.registers[7] = not_taken.1;
            let outcome = execute_native(&predicted_taken, &mut actual);
            assert!(!outcome.needs_interpreter());
            assert_eq!(outcome.retired(), 2);
            assert!((outcome.retired() as usize) < predicted_taken.instruction_count());
            assert_eq!(outcome.next_pc(), IMAGE_START + 8);
            assert_eq!(actual.registers[5], 11);

            let mut actual = machine_with_code(&code, IMAGE_START);
            actual.registers[5] = 10;
            actual.registers[6] = not_taken.0;
            actual.registers[7] = not_taken.1;
            let outcome = execute_native(&predicted_fallthrough, &mut actual);
            assert!(!outcome.needs_interpreter());
            assert_eq!(outcome.retired(), 3);
            assert_eq!(
                outcome.retired() as usize,
                predicted_fallthrough.instruction_count()
            );
            assert_eq!(outcome.next_pc(), IMAGE_START + 12);
            assert_eq!(actual.registers[5], 13);

            let mut actual = machine_with_code(&code, IMAGE_START);
            actual.registers[5] = 10;
            actual.registers[6] = taken.0;
            actual.registers[7] = taken.1;
            let outcome = execute_native(&predicted_fallthrough, &mut actual);
            assert!(!outcome.needs_interpreter());
            assert_eq!(outcome.retired(), 2);
            assert!((outcome.retired() as usize) < predicted_fallthrough.instruction_count());
            assert_eq!(outcome.next_pc(), IMAGE_START + 16);
            assert_eq!(actual.registers[5], 11);
        }
    }

    #[test]
    fn four_copy_unroll_matches_four_complete_self_loop_iterations() {
        let code = [addi(5, 5, 1), branch(4, 5, 6, -4)];
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_unrolled_region(
            &template,
            &[
                (IMAGE_START, 2),
                (IMAGE_START, 2),
                (IMAGE_START, 2),
                (IMAGE_START, 2),
            ],
        );
        let mut expected = machine_with_code(&code, IMAGE_START);
        let mut actual = machine_with_code(&code, IMAGE_START);
        expected.registers[6] = 5;
        actual.registers[6] = 5;

        for _ in 0..native.instruction_count() {
            let instruction = expected.fetch_decode(expected.pc);
            assert!(expected.execute_one(instruction).is_none());
        }
        let outcome = execute_native(&native, &mut actual);
        actual.pc = outcome.next_pc();
        actual.retired += u64::from(outcome.retired());

        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 8);
        assert_eq!(outcome.next_pc(), IMAGE_START);
        assert_eq!(actual.registers[5], 4);
        assert_eq!(actual.registers, expected.registers);
        assert_eq!(actual.pc, expected.pc);
        assert_eq!(actual.retired, expected.retired);
    }

    #[test]
    fn unrolled_guard_exit_reports_early_iteration_state_exactly() {
        let code = [addi(5, 5, 1), branch(4, 5, 6, -4)];
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_unrolled_region(
            &template,
            &[
                (IMAGE_START, 2),
                (IMAGE_START, 2),
                (IMAGE_START, 2),
                (IMAGE_START, 2),
            ],
        );
        let mut expected = machine_with_code(&code, IMAGE_START);
        let mut actual = machine_with_code(&code, IMAGE_START);
        expected.registers[5] = 10;
        actual.registers[5] = 10;
        expected.registers[6] = 12;
        actual.registers[6] = 12;

        for _ in 0..4 {
            let instruction = expected.fetch_decode(expected.pc);
            assert!(expected.execute_one(instruction).is_none());
        }
        let outcome = execute_native(&native, &mut actual);
        actual.pc = outcome.next_pc();
        actual.retired += u64::from(outcome.retired());

        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 4);
        assert_eq!(outcome.next_pc(), IMAGE_START + 8);
        assert_eq!(actual.registers[5], 12);
        assert_eq!(actual.registers, expected.registers);
        assert_eq!(actual.pc, expected.pc);
        assert_eq!(actual.retired, expected.retired);
    }

    #[test]
    fn later_unrolled_iteration_side_exit_is_precise() {
        let code = [load(7, 6, 2, 0), addi(6, 6, 4), branch(6, 6, 8, -8)];
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_unrolled_region(
            &template,
            &[
                (IMAGE_START, 3),
                (IMAGE_START, 3),
                (IMAGE_START, 3),
                (IMAGE_START, 3),
            ],
        );
        let first_unreadable = IMAGE_START + PAGE_SIZE as u32;
        let mut expected = machine_with_code(&code, IMAGE_START);
        let mut actual = machine_with_code(&code, IMAGE_START);
        for machine in [&mut expected, &mut actual] {
            machine.registers[6] = first_unreadable - 8;
            machine.registers[7] = 0xdead_beef;
            machine.registers[8] = first_unreadable + 16;
        }

        for _ in 0..6 {
            let instruction = expected.fetch_decode(expected.pc);
            assert!(expected.execute_one(instruction).is_none());
        }
        let outcome = execute_native(&native, &mut actual);
        actual.pc = outcome.next_pc();
        actual.retired += u64::from(outcome.retired());

        assert!(outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 6);
        assert_eq!(outcome.next_pc(), IMAGE_START);
        assert_eq!(actual.registers[6], first_unreadable);
        assert_eq!(actual.registers[7], 0);
        assert_eq!(actual.registers, expected.registers);
        assert_eq!(actual.pc, expected.pc);
        assert_eq!(actual.retired, expected.retired);

        let expected_instruction = expected.fetch_decode(expected.pc);
        let actual_instruction = actual.fetch_decode(actual.pc);
        let Some(Termination::Trap(expected_trap)) = expected.execute_one(expected_instruction)
        else {
            panic!("third repeated load must trap in the interpreter");
        };
        let Some(Termination::Trap(actual_trap)) = actual.execute_one(actual_instruction) else {
            panic!("unrolled side-exit fallback must reproduce the trap");
        };
        assert_eq!(actual_trap, expected_trap);
        assert_eq!(actual_trap.cause, "LoadAccessFault");
        assert_eq!(actual.retired, 6);
    }

    #[test]
    fn counted_loop_honors_complete_group_budget_boundaries_without_early_mutation() {
        let code = [addi(5, 5, 1), branch(4, 5, 6, -4)];
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_grouped_loop(&template, &[(IMAGE_START, 2)], 4);
        assert_eq!(native.kind(), NativeEntryKind::Loop);
        assert_eq!(native.instruction_count(), 2);
        assert_eq!(native.minimum_instruction_count(), 8);
        assert_eq!(native.loop_unroll_factor(), 4);

        for remaining in [0, 1, 2, 3, 7, 8, 9, 15, 16, 19] {
            let mut expected = machine_with_code(&code, IMAGE_START);
            let mut actual = machine_with_code(&code, IMAGE_START);
            expected.registers[6] = 100;
            actual.registers[6] = 100;
            actual
                .memory
                .store(STACK_START, 4, 0xaabb_ccdd, IMAGE_START)
                .unwrap();
            let before_registers = actual.registers;
            let expected_retired = remaining / 8 * 8;
            for _ in 0..expected_retired {
                let instruction = expected.fetch_decode(expected.pc);
                assert!(expected.execute_one(instruction).is_none());
            }

            let outcome = execute_loop(&native, &mut actual, remaining);
            if expected_retired == 0 {
                assert!(outcome.is_none());
                assert_eq!(actual.registers, before_registers);
                assert_eq!(actual.memory.load_u32(STACK_START), 0xaabb_ccdd);
                continue;
            }
            let outcome = outcome.unwrap();
            actual.pc = outcome.next_pc();
            actual.retired = u64::from(outcome.retired());
            assert!(!outcome.needs_interpreter());
            assert_eq!(outcome.retired(), expected_retired as u32);
            assert_eq!(actual.pc, IMAGE_START);
            assert_eq!(actual.registers, expected.registers);
            assert_eq!(actual.retired, expected.retired);
        }
    }

    #[test]
    fn counted_loop_guard_retirement_is_exact_in_each_physical_copy() {
        let code = [
            addi(5, 5, 1),
            branch(0, 5, 6, 12),
            addi(7, 7, 1),
            jal(0, -12),
            NOP,
        ];
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_grouped_loop(
            &template,
            &[(IMAGE_START, 2), (IMAGE_START.wrapping_add(8), 2)],
            4,
        );
        assert_eq!(native.instruction_count(), 4);
        assert_eq!(native.minimum_instruction_count(), 16);

        for trigger_copy in 1..=4 {
            let expected_retired = (trigger_copy - 1) * 4 + 2;
            let mut expected = machine_with_code(&code, IMAGE_START);
            let mut actual = machine_with_code(&code, IMAGE_START);
            expected.registers[6] = trigger_copy;
            actual.registers[6] = trigger_copy;
            for _ in 0..expected_retired {
                let instruction = expected.fetch_decode(expected.pc);
                assert!(expected.execute_one(instruction).is_none());
            }

            let outcome = execute_loop(&native, &mut actual, 32).unwrap();
            actual.pc = outcome.next_pc();
            actual.retired = u64::from(outcome.retired());
            assert!(!outcome.needs_interpreter());
            assert_eq!(outcome.retired(), expected_retired);
            assert_eq!(outcome.next_pc(), IMAGE_START + 16);
            assert_eq!(actual.registers[5], trigger_copy);
            assert_eq!(actual.registers[7], trigger_copy - 1);
            assert_eq!(actual.registers, expected.registers);
            assert_eq!(actual.pc, expected.pc);
            assert_eq!(actual.retired, expected.retired);
        }
    }

    #[test]
    fn counted_loop_six_slot_cache_preserves_multi_cycle_state_and_callee_abi() {
        let mut code = six_hot_register_updates();
        code.push(jal(0, -48));
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_loop(&template, &[(IMAGE_START, code.len())]);
        let mut expected = machine_with_code(&code, IMAGE_START);
        let mut actual = machine_with_code(&code, IMAGE_START);
        for register in 5..=10 {
            let value = register as u32 * 10;
            expected.registers[register] = value;
            actual.registers[register] = value;
        }
        for _ in 0..104 {
            let instruction = expected.fetch_decode(expected.pc);
            assert!(expected.execute_one(instruction).is_none());
        }

        let (outcome, observed) =
            execute_loop_with_callee_sentinels(&native, &mut actual, 104).unwrap();
        actual.pc = outcome.next_pc();
        actual.retired = u64::from(outcome.retired());

        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 104);
        assert_eq!(observed, CALLEE_SENTINELS);
        assert_eq!(actual.registers, expected.registers);
        assert_eq!(actual.pc, expected.pc);
        assert_eq!(actual.retired, expected.retired);
    }

    #[test]
    fn counted_loop_r9_scratch_keeps_five_cached_hosts_across_cycles() {
        let mut code = Vec::new();
        for register in 5..=9 {
            code.push(addi(register, register, 1));
            code.push(addi(register, register, 1));
        }
        code.push(register(20, 21, 22, 2, 1)); // mulhsu reserves r9d as scratch
        code.push(jal(0, -44));
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_loop(&template, &[(IMAGE_START, code.len())]);
        let mut expected = machine_with_code(&code, IMAGE_START);
        let mut actual = machine_with_code(&code, IMAGE_START);
        for machine in [&mut expected, &mut actual] {
            machine.registers[21] = 0x8765_4321;
            machine.registers[22] = 0x1020_3040;
        }
        for _ in 0..96 {
            let instruction = expected.fetch_decode(expected.pc);
            assert!(expected.execute_one(instruction).is_none());
        }

        let (outcome, observed) =
            execute_loop_with_callee_sentinels(&native, &mut actual, 96).unwrap();
        actual.pc = outcome.next_pc();
        actual.retired = u64::from(outcome.retired());

        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 96);
        assert_eq!(observed, CALLEE_SENTINELS);
        assert_eq!(actual.registers, expected.registers);
        assert_eq!(actual.pc, expected.pc);
        assert_eq!(actual.retired, expected.retired);
    }

    #[test]
    fn counted_loop_six_slot_guard_exit_spills_state_and_restores_callee_hosts() {
        let mut code = six_hot_register_updates();
        code.push(branch(4, 5, 20, -48));
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_loop(&template, &[(IMAGE_START, code.len())]);
        let mut expected = machine_with_code(&code, IMAGE_START);
        let mut actual = machine_with_code(&code, IMAGE_START);
        expected.registers[20] = 8;
        actual.registers[20] = 8;
        for _ in 0..52 {
            let instruction = expected.fetch_decode(expected.pc);
            assert!(expected.execute_one(instruction).is_none());
        }

        let (outcome, observed) =
            execute_loop_with_callee_sentinels(&native, &mut actual, 104).unwrap();
        actual.pc = outcome.next_pc();
        actual.retired = u64::from(outcome.retired());

        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 52);
        assert_eq!(outcome.next_pc(), IMAGE_START + 52);
        assert_eq!(observed, CALLEE_SENTINELS);
        assert_eq!(actual.registers, expected.registers);
        assert_eq!(actual.pc, expected.pc);
        assert_eq!(actual.retired, expected.retired);
    }

    #[test]
    fn counted_loop_six_slot_side_exit_spills_state_and_restores_callee_hosts() {
        let mut code = six_hot_register_updates();
        code.push(load(20, 21, 2, 0));
        code.push(addi(21, 21, 4));
        code.push(jal(0, -56));
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_loop(&template, &[(IMAGE_START, code.len())]);
        let mut expected = machine_with_code(&code, IMAGE_START);
        let mut actual = machine_with_code(&code, IMAGE_START);
        for machine in [&mut expected, &mut actual] {
            machine.registers[20] = 0xdead_beef;
            machine.registers[21] = STACK_END - 20;
        }
        for _ in 0..87 {
            let instruction = expected.fetch_decode(expected.pc);
            assert!(expected.execute_one(instruction).is_none());
        }

        let (outcome, observed) =
            execute_loop_with_callee_sentinels(&native, &mut actual, 120).unwrap();
        actual.pc = outcome.next_pc();
        actual.retired = u64::from(outcome.retired());

        assert!(outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 87);
        assert_eq!(outcome.next_pc(), IMAGE_START + 48);
        assert_eq!(observed, CALLEE_SENTINELS);
        assert_eq!(actual.registers, expected.registers);
        assert_eq!(actual.pc, expected.pc);
        assert_eq!(actual.retired, expected.retired);

        let expected_instruction = expected.fetch_decode(expected.pc);
        let actual_instruction = actual.fetch_decode(actual.pc);
        let Some(Termination::Trap(expected_trap)) = expected.execute_one(expected_instruction)
        else {
            panic!("later six-slot loop load must trap in the interpreter");
        };
        let Some(Termination::Trap(actual_trap)) = actual.execute_one(actual_instruction) else {
            panic!("six-slot side exit must reproduce the memory trap");
        };
        assert_eq!(actual_trap, expected_trap);
        assert_eq!(actual.registers, expected.registers);
        assert_eq!(actual.retired, expected.retired);
    }

    #[test]
    fn loop_only_register_choices_preserve_counted_state() {
        let read_code = [branch(1, 5, 0, 0)];
        let template = machine_with_code(&read_code, IMAGE_START);
        let native = native_loop(&template, &[(IMAGE_START, 1)]);
        let mut actual = machine_with_code(&read_code, IMAGE_START);
        actual.registers[5] = 9;
        let outcome = execute_loop(&native, &mut actual, 4).unwrap();
        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 4);
        assert_eq!(outcome.next_pc(), IMAGE_START);
        assert_eq!(actual.registers[5], 9);

        let read_write_code = [addi(5, 5, 1), jal(0, -4)];
        let template = machine_with_code(&read_write_code, IMAGE_START);
        let native = native_loop(&template, &[(IMAGE_START, 2)]);
        let mut actual = machine_with_code(&read_write_code, IMAGE_START);
        actual.registers[5] = 41;
        let outcome = execute_loop(&native, &mut actual, 8).unwrap();
        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 8);
        assert_eq!(outcome.next_pc(), IMAGE_START);
        assert_eq!(actual.registers[5], 45);
    }

    #[test]
    fn counted_loop_guard_keeps_prior_lap_cached_state() {
        let code = [branch(0, 5, 6, 12), addi(5, 5, 1), jal(0, -8), NOP];
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_loop(
            &template,
            &[(IMAGE_START, 1), (IMAGE_START.wrapping_add(4), 2)],
        );
        let mut expected = machine_with_code(&code, IMAGE_START);
        let mut actual = machine_with_code(&code, IMAGE_START);
        expected.registers[6] = 1;
        actual.registers[6] = 1;
        for _ in 0..4 {
            let instruction = expected.fetch_decode(expected.pc);
            assert!(expected.execute_one(instruction).is_none());
        }

        let outcome = execute_loop(&native, &mut actual, 30).unwrap();
        actual.pc = outcome.next_pc();
        actual.retired = u64::from(outcome.retired());
        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 4);
        assert_eq!(outcome.next_pc(), IMAGE_START + 12);
        assert_eq!(actual.registers[5], 1);
        assert_eq!(actual.registers, expected.registers);
        assert_eq!(actual.pc, expected.pc);
        assert_eq!(actual.retired, expected.retired);
    }

    #[test]
    fn counted_loop_zero_left_guard_spills_prior_direct_state() {
        let code = [addi(6, 6, 1), branch(6, 0, 5, -4), NOP];
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_loop(&template, &[(IMAGE_START, 2)]);
        let mut actual = machine_with_code(&code, IMAGE_START);
        actual.registers[5] = 0;
        actual.registers[6] = 41;

        let outcome = execute_loop(&native, &mut actual, 20).unwrap();
        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 2);
        assert_eq!(outcome.next_pc(), IMAGE_START + 8);
        assert_eq!(actual.registers[6], 42);
    }

    #[test]
    fn counted_loop_copy_four_guard_can_exit_at_exact_group_to_nonhead_pc() {
        let code = [addi(5, 5, 1), branch(4, 5, 6, -4), NOP];
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_grouped_loop(
            &template,
            &[(IMAGE_START, 1), (IMAGE_START.wrapping_add(4), 1)],
            4,
        );
        let mut expected = machine_with_code(&code, IMAGE_START);
        let mut actual = machine_with_code(&code, IMAGE_START);
        expected.registers[6] = 4;
        actual.registers[6] = 4;
        for _ in 0..8 {
            let instruction = expected.fetch_decode(expected.pc);
            assert!(expected.execute_one(instruction).is_none());
        }

        let outcome = execute_loop(&native, &mut actual, 20).unwrap();
        actual.pc = outcome.next_pc();
        actual.retired = u64::from(outcome.retired());
        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 8);
        assert_eq!(outcome.retired(), native.minimum_instruction_count() as u32);
        assert_eq!(outcome.next_pc(), IMAGE_START + 8);
        assert_eq!(actual.registers, expected.registers);
        assert_eq!(actual.pc, expected.pc);
        assert_eq!(actual.retired, expected.retired);
    }

    #[test]
    fn counted_loop_later_memory_side_exit_is_precise() {
        let code = [addi(5, 5, 1), load(7, 6, 2, 0), addi(6, 6, 4), jal(0, -12)];
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_loop(&template, &[(IMAGE_START, 4)]);

        for (base, remaining, expected_retired) in
            [(STACK_END - 12, 16, 13), (STACK_END - 20, 32, 21)]
        {
            let mut expected = machine_with_code(&code, IMAGE_START);
            let mut actual = machine_with_code(&code, IMAGE_START);
            for machine in [&mut expected, &mut actual] {
                machine.registers[6] = base;
                machine.registers[7] = 0xdead_beef;
            }
            for _ in 0..expected_retired {
                let instruction = expected.fetch_decode(expected.pc);
                assert!(expected.execute_one(instruction).is_none());
            }

            let outcome = execute_loop(&native, &mut actual, remaining).unwrap();
            actual.pc = outcome.next_pc();
            actual.retired = u64::from(outcome.retired());
            assert!(outcome.needs_interpreter());
            assert_eq!(outcome.retired(), expected_retired);
            assert_eq!(outcome.next_pc(), IMAGE_START + 4);
            assert_eq!(actual.registers[6], STACK_END);
            assert_eq!(actual.registers[7], 0);
            assert_eq!(actual.registers, expected.registers);
            assert_eq!(actual.retired, expected.retired);

            let expected_instruction = expected.fetch_decode(expected.pc);
            let actual_instruction = actual.fetch_decode(actual.pc);
            let Some(Termination::Trap(expected_trap)) = expected.execute_one(expected_instruction)
            else {
                panic!("later grouped-loop load must trap in the interpreter");
            };
            let Some(Termination::Trap(actual_trap)) = actual.execute_one(actual_instruction)
            else {
                panic!("grouped-loop side exit must reproduce the memory trap");
            };
            assert_eq!(actual_trap, expected_trap);
            assert_eq!(actual.registers, expected.registers);
            assert_eq!(actual.retired, expected.retired);
        }
    }

    #[test]
    fn counted_loop_later_division_exit_is_precise() {
        let code = [addi(6, 6, -1), register(5, 7, 6, 4, 1), jal(0, -8)];
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_loop(&template, &[(IMAGE_START, 3)]);
        let mut expected = machine_with_code(&code, IMAGE_START);
        let mut actual = machine_with_code(&code, IMAGE_START);
        for machine in [&mut expected, &mut actual] {
            machine.registers[5] = 0xfeed_face;
            machine.registers[6] = 6;
            machine.registers[7] = 10;
        }
        for _ in 0..16 {
            let instruction = expected.fetch_decode(expected.pc);
            assert!(expected.execute_one(instruction).is_none());
        }

        let outcome = execute_loop(&native, &mut actual, 24).unwrap();
        actual.pc = outcome.next_pc();
        actual.retired = u64::from(outcome.retired());
        assert!(outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 16);
        assert_eq!(outcome.next_pc(), IMAGE_START + 4);
        assert_eq!(actual.registers[5], 10);
        assert_eq!(actual.registers[6], 0);
        assert_eq!(actual.registers, expected.registers);

        let expected_instruction = expected.fetch_decode(expected.pc);
        let actual_instruction = actual.fetch_decode(actual.pc);
        assert!(expected.execute_one(expected_instruction).is_none());
        assert!(actual.execute_one(actual_instruction).is_none());
        assert_eq!(actual.registers, expected.registers);
        assert_eq!(actual.pc, expected.pc);
        assert_eq!(actual.retired, expected.retired);
    }

    #[test]
    fn counted_loop_store_is_visible_to_the_next_iteration_load() {
        let code = [
            load(7, 6, 2, 0),
            addi(7, 7, 1),
            store(7, 6, 2, 0),
            jal(0, -12),
        ];
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_loop(&template, &[(IMAGE_START, 4)]);
        for remaining in [16, 32] {
            let mut expected = machine_with_code(&code, IMAGE_START);
            let mut actual = machine_with_code(&code, IMAGE_START);
            for machine in [&mut expected, &mut actual] {
                machine.registers[6] = STACK_START;
                machine
                    .memory
                    .store(STACK_START, 4, 10, IMAGE_START)
                    .unwrap();
            }
            for _ in 0..remaining {
                let instruction = expected.fetch_decode(expected.pc);
                assert!(expected.execute_one(instruction).is_none());
            }

            let outcome = execute_loop(&native, &mut actual, remaining).unwrap();
            actual.pc = outcome.next_pc();
            actual.retired = u64::from(outcome.retired());
            assert!(!outcome.needs_interpreter());
            assert_eq!(outcome.retired(), remaining as u32);
            assert_eq!(actual.registers, expected.registers);
            assert_eq!(
                actual.memory.load_u32(STACK_START),
                expected.memory.load_u32(STACK_START)
            );
            assert_eq!(actual.pc, expected.pc);
            assert_eq!(actual.retired, expected.retired);
        }
    }

    #[test]
    fn counted_loop_large_budget_is_capped_below_side_exit_flag() {
        let quantum = 8;
        let iterations = loop_iteration_budget(quantum, u64::MAX).unwrap();
        assert_eq!(iterations, (SIDE_EXIT_FLAG - 1) / quantum as u32);
        assert!(u64::from(iterations) * (quantum as u64) < u64::from(SIDE_EXIT_FLAG));
        assert!(u64::from(iterations + 1) * (quantum as u64) >= u64::from(SIDE_EXIT_FLAG));

        // A first-iteration guard makes an invocation with the maximum budget
        // complete immediately while exercising the encoded iteration cap.
        let code = [branch(1, 0, 0, 0)];
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_loop(&template, &[(IMAGE_START, 1)]);
        assert_eq!(native.minimum_instruction_count(), 1);
        let mut actual = machine_with_code(&code, IMAGE_START);
        let outcome = execute_loop(&native, &mut actual, u64::MAX).unwrap();
        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 1);
        assert_eq!(outcome.next_pc(), IMAGE_START + 4);
    }

    #[test]
    fn predicted_jal_keeps_cached_link_precise_on_successor_side_exit() {
        let code = [
            addi(5, 5, 1),
            jal(1, 12),
            NOP,
            NOP,
            addi(1, 1, 4),
            load(9, 6, 2, 1),
        ];
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_region(
            &template,
            &[(IMAGE_START, 2), (IMAGE_START.wrapping_add(16), 2)],
        );
        let mut actual = machine_with_code(&code, IMAGE_START);
        actual.registers[1] = 0xfeed_face;
        actual.registers[5] = 10;
        actual.registers[6] = STACK_START;
        actual.registers[9] = 0xdead_beef;

        let outcome = execute_native(&native, &mut actual);

        assert!(outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 3);
        assert!((outcome.retired() as usize) < native.instruction_count());
        assert_eq!(outcome.next_pc(), IMAGE_START + 20);
        assert_eq!(actual.registers[1], IMAGE_START + 12);
        assert_eq!(actual.registers[5], 11);
        assert_eq!(actual.registers[9], 0xdead_beef);
    }

    #[test]
    fn predicted_fallthrough_preserves_misaligned_taken_branch_trap() {
        let code = [addi(5, 5, 1), branch(0, 6, 7, 2), addi(5, 5, 4)];
        let template = machine_with_code(&code, IMAGE_START);
        let native = native_region(
            &template,
            &[(IMAGE_START, 2), (IMAGE_START.wrapping_add(8), 1)],
        );

        let mut actual = machine_with_code(&code, IMAGE_START);
        actual.registers[5] = 10;
        actual.registers[6] = 1;
        actual.registers[7] = 2;
        let outcome = execute_native(&native, &mut actual);
        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 3);
        assert_eq!(outcome.next_pc(), IMAGE_START + 12);
        assert_eq!(actual.registers[5], 15);

        let mut actual = machine_with_code(&code, IMAGE_START);
        actual.registers[5] = 10;
        actual.registers[6] = 1;
        actual.registers[7] = 1;
        let outcome = execute_native(&native, &mut actual);
        assert!(outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 1);
        assert_eq!(outcome.next_pc(), IMAGE_START + 4);
        assert_eq!(actual.registers[5], 11);
    }

    #[test]
    fn exceptional_division_side_exits_before_mutation() {
        let cases = [
            (4, 123, 0),
            (5, 123, 0),
            (6, 123, 0),
            (7, 123, 0),
            (4, 0x8000_0000, u32::MAX),
            (6, 0x8000_0000, u32::MAX),
        ];

        for (funct3, left, right) in cases {
            let code = [addi(8, 8, 1), register(5, 6, 7, funct3, 1), addi(9, 9, 1)];
            let native = native_block(&code);
            let mut expected = machine_with_code(&code, IMAGE_START);
            let mut actual = machine_with_code(&code, IMAGE_START);
            for machine in [&mut expected, &mut actual] {
                machine.registers[5] = 0xfeed_face;
                machine.registers[6] = left;
                machine.registers[7] = right;
            }

            let first = expected.fetch_decode(expected.pc);
            assert!(expected.execute_one(first).is_none());
            let second = expected.fetch_decode(expected.pc);
            assert!(expected.execute_one(second).is_none());

            let outcome = execute_native(&native, &mut actual);
            actual.pc = outcome.next_pc();
            actual.retired += u64::from(outcome.retired());
            assert!(outcome.needs_interpreter());
            assert_eq!(outcome.retired(), 1);
            assert_eq!(actual.pc, IMAGE_START + 4);
            assert_eq!(actual.registers[5], 0xfeed_face);

            let fallback = actual.fetch_decode(actual.pc);
            assert!(actual.execute_one(fallback).is_none());
            assert_eq!(actual.pc, expected.pc);
            assert_eq!(actual.retired, expected.retired);
            assert_eq!(actual.registers, expected.registers);
        }

        let code = [addi(5, 5, 1), addi(5, 5, 2), register(5, 5, 0, 4, 1)];
        let native = native_block(&code);
        let mut actual = machine_with_code(&code, IMAGE_START);
        actual.registers[5] = 10;

        let outcome = execute_native(&native, &mut actual);
        assert!(outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 2);
        assert_eq!(actual.registers[5], 13);
    }

    #[test]
    fn executes_load_widths_signedness_and_implicit_zero_pages() {
        let cases = [
            (0, 0, 0x0000_0001),
            (0, 2, 0xffff_ffff),
            (0, 3, 0xffff_ff80),
            (1, 0, 0x0000_7f01),
            (1, 2, 0xffff_80ff),
            (2, 0, 0x80ff_7f01),
            (4, 2, 0x0000_00ff),
            (4, 3, 0x0000_0080),
            (5, 2, 0x0000_80ff),
        ];

        for (funct3, offset, value) in cases {
            let code = [load(5, 6, funct3, offset), NOP];
            let native = native_block(&code);
            let mut expected = machine_with_code(&code, IMAGE_START);
            let mut actual = machine_with_code(&code, IMAGE_START);
            for machine in [&mut expected, &mut actual] {
                machine.registers[6] = STACK_START;
                machine
                    .memory
                    .store(STACK_START, 4, 0x80ff_7f01, IMAGE_START)
                    .unwrap();
            }
            for _ in 0..native.instruction_count() {
                let instruction = expected.fetch_decode(expected.pc);
                assert!(expected.execute_one(instruction).is_none());
            }

            let outcome = execute_native(&native, &mut actual);
            assert!(!outcome.needs_interpreter());
            assert_eq!(actual.registers, expected.registers);
            assert_eq!(actual.registers[5], value);
        }

        let code = [load(5, 6, 2, 0), NOP];
        let native = native_block(&code);
        let mut machine = machine_with_code(&code, IMAGE_START);
        machine.registers[5] = u32::MAX;
        machine.registers[6] = STACK_START;
        let outcome = execute_native(&native, &mut machine);
        assert!(!outcome.needs_interpreter());
        assert_eq!(machine.registers[5], 0);
    }

    #[test]
    fn executes_store_widths_on_initialized_flat_memory() {
        let cases = [
            (0, 0x4433_22aa, 0x4433_22aa),
            (1, 0x4433_bbaa, 0x4433_bbaa),
            (2, 0xddcc_bbaa, 0xddcc_bbaa),
        ];

        for (funct3, source, expected_word) in cases {
            let code = [store(7, 6, funct3, 0), NOP];
            let native = native_block(&code);
            let mut expected = machine_with_code(&code, IMAGE_START);
            let mut actual = machine_with_code(&code, IMAGE_START);
            for machine in [&mut expected, &mut actual] {
                machine.registers[6] = STACK_START;
                machine.registers[7] = source;
                machine
                    .memory
                    .store(STACK_START, 4, 0x4433_2211, IMAGE_START)
                    .unwrap();
            }
            for _ in 0..native.instruction_count() {
                let instruction = expected.fetch_decode(expected.pc);
                assert!(expected.execute_one(instruction).is_none());
            }

            let outcome = execute_native(&native, &mut actual);
            assert!(!outcome.needs_interpreter());
            assert_eq!(actual.memory.load_u32(STACK_START), expected_word);
            assert_eq!(
                actual.memory.load_u32(STACK_START),
                expected.memory.load_u32(STACK_START)
            );
        }
    }

    #[test]
    fn cached_memory_sources_and_destinations_match_the_interpreter() {
        for funct3 in [0, 1, 2, 4, 5] {
            let code = [
                addi(5, 5, 1),
                addi(5, 5, 2),
                load(5, 6, funct3, 0),
                addi(5, 5, 3),
                NOP,
            ];
            let native = native_block(&code);
            let mut expected = machine_with_code(&code, IMAGE_START);
            let mut actual = machine_with_code(&code, IMAGE_START);
            for machine in [&mut expected, &mut actual] {
                machine.registers[5] = 10;
                machine.registers[6] = STACK_START;
                machine
                    .memory
                    .store(STACK_START, 4, 0x80ff_7f01, IMAGE_START)
                    .unwrap();
            }
            for _ in 0..native.instruction_count() {
                let instruction = expected.fetch_decode(expected.pc);
                assert!(expected.execute_one(instruction).is_none());
            }

            let outcome = execute_native(&native, &mut actual);
            assert!(!outcome.needs_interpreter());
            assert_eq!(actual.registers, expected.registers);
        }

        for funct3 in [0, 1, 2] {
            let code = [
                addi(5, 5, 1),
                addi(5, 5, 2),
                store(5, 6, funct3, 0),
                addi(5, 5, 3),
                NOP,
            ];
            let native = native_block(&code);
            let mut expected = machine_with_code(&code, IMAGE_START);
            let mut actual = machine_with_code(&code, IMAGE_START);
            for machine in [&mut expected, &mut actual] {
                machine.registers[5] = 0x4433_22aa;
                machine.registers[6] = STACK_START;
                machine
                    .memory
                    .store(STACK_START, 4, 0x8877_6655, IMAGE_START)
                    .unwrap();
            }
            for _ in 0..native.instruction_count() {
                let instruction = expected.fetch_decode(expected.pc);
                assert!(expected.execute_one(instruction).is_none());
            }

            let outcome = execute_native(&native, &mut actual);
            assert!(!outcome.needs_interpreter());
            assert_eq!(actual.registers, expected.registers);
            assert_eq!(
                actual.memory.load_u32(STACK_START),
                expected.memory.load_u32(STACK_START)
            );
        }
    }

    #[test]
    fn highest_legal_addresses_execute_flat_loads_and_stores() {
        let value = 0x4433_22aa;
        let cases = [
            (0, 4, ADDRESS_SPACE_SIZE - 1, 0xaa),
            (1, 5, ADDRESS_SPACE_SIZE - 2, 0x22aa),
            (2, 2, ADDRESS_SPACE_SIZE - 4, value),
        ];

        for (store_width, load_width, address, expected_value) in cases {
            let code = [store(7, 6, store_width, 0), load(5, 6, load_width, 0), NOP];
            let native = native_block(&code);
            let mut expected = machine_with_code(&code, IMAGE_START);
            let mut actual = machine_with_code(&code, IMAGE_START);
            for machine in [&mut expected, &mut actual] {
                machine.registers[6] = address;
                machine.registers[7] = value;
            }
            for _ in 0..native.instruction_count() {
                let instruction = expected.fetch_decode(expected.pc);
                assert!(expected.execute_one(instruction).is_none());
            }

            let outcome = execute_native(&native, &mut actual);
            assert!(!outcome.needs_interpreter());
            assert_eq!(outcome.retired(), 3);
            assert_eq!(actual.registers, expected.registers);
            assert_eq!(actual.registers[5], expected_value);
        }
    }

    #[test]
    fn precise_side_exits_spill_prior_cached_values_without_committing_destination() {
        let code = [addi(5, 5, 1), addi(5, 5, 2), load(5, 6, 2, 1)];
        let native = native_block(&code);
        let mut actual = machine_with_code(&code, IMAGE_START);
        actual.registers[5] = 10;
        actual.registers[6] = STACK_START;

        let outcome = execute_native(&native, &mut actual);
        assert!(outcome.needs_interpreter());
        assert_eq!(outcome.next_pc(), IMAGE_START + 8);
        assert_eq!(outcome.retired(), 2);
        assert_eq!(actual.registers[5], 13);

        actual.pc = outcome.next_pc();
        actual.retired = u64::from(outcome.retired());
        let fallback = actual.fetch_decode(actual.pc);
        let Some(Termination::Trap(trap)) = actual.execute_one(fallback) else {
            panic!("misaligned cached load must trap");
        };
        assert_eq!(trap.cause, "LoadAddressMisaligned");
        assert_eq!(actual.registers[5], 13);

        let code = [addi(5, 5, 1), addi(5, 5, 2), store(5, 6, 2, 1)];
        let native = native_block(&code);
        let mut actual = machine_with_code(&code, IMAGE_START);
        actual.registers[5] = 10;
        actual.registers[6] = STACK_START;

        let outcome = execute_native(&native, &mut actual);
        assert!(outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 2);
        assert_eq!(actual.registers[5], 13);
        actual.pc = outcome.next_pc();
        actual.retired = u64::from(outcome.retired());
        let fallback = actual.fetch_decode(actual.pc);
        let Some(Termination::Trap(trap)) = actual.execute_one(fallback) else {
            panic!("misaligned cached store must trap");
        };
        assert_eq!(trap.cause, "StoreAddressMisaligned");
        assert_eq!(actual.memory.load_u32(STACK_START), 0);

        let code = [
            addi(5, 5, 1),
            addi(5, 5, 2),
            addi(6, 6, 3),
            addi(6, 6, 4),
            addi(7, 7, 5),
            addi(7, 7, 6),
            load(5, 8, 2, 1),
        ];
        let native = native_block(&code);
        let mut actual = machine_with_code(&code, IMAGE_START);
        actual.registers[5] = 10;
        actual.registers[6] = 20;
        actual.registers[7] = 30;
        actual.registers[8] = STACK_START;

        let outcome = execute_native(&native, &mut actual);
        assert!(outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 6);
        assert_eq!(actual.registers[5], 13);
        assert_eq!(actual.registers[6], 27);
        assert_eq!(actual.registers[7], 41);
    }

    #[test]
    fn pristine_stack_store_stays_native_and_is_visible_to_later_entries() {
        let code = [addi(8, 8, 1), store(7, 6, 2, 0), addi(9, 9, 1)];
        let native = native_block(&code);
        let mut expected = machine_with_code(&code, IMAGE_START);
        let mut actual = machine_with_code(&code, IMAGE_START);
        for machine in [&mut expected, &mut actual] {
            machine.registers[6] = STACK_START;
            machine.registers[7] = 0x4433_2211;
        }
        for _ in 0..native.instruction_count() {
            let instruction = expected.fetch_decode(expected.pc);
            assert!(expected.execute_one(instruction).is_none());
        }

        let outcome = execute_native(&native, &mut actual);
        actual.pc = outcome.next_pc();
        actual.retired += u64::from(outcome.retired());
        assert!(!outcome.needs_interpreter());
        assert_eq!(outcome.next_pc(), IMAGE_START + 12);
        assert_eq!(outcome.retired(), 3);
        assert_eq!(actual.registers, expected.registers);
        assert_eq!(actual.memory.load_u32(STACK_START), 0x4433_2211);
        assert_eq!(actual.retired, expected.retired);

        // Direct native stores update the canonical flat buffer observed by a
        // later entry in the same run.
        let load_code = [load(10, 6, 2, 0), NOP];
        let load_native = native_block(&load_code);
        let load_outcome = execute_native(&load_native, &mut actual);
        assert!(!load_outcome.needs_interpreter());
        assert_eq!(actual.registers[10], 0x4433_2211);
    }

    #[test]
    fn memory_fault_side_exits_preserve_destination_and_memory() {
        let cases = [
            (
                load(5, 6, 2, 1),
                STACK_START,
                "LoadAddressMisaligned",
                STACK_START + 1,
            ),
            (load(5, 6, 2, 0), 0, "LoadAccessFault", 0),
            (
                load(5, 6, 2, 0),
                ADDRESS_SPACE_SIZE,
                "LoadAccessFault",
                ADDRESS_SPACE_SIZE,
            ),
            // The immediate addition wraps to zero before range and
            // permission checks, matching RV32 arithmetic.
            (load(5, 6, 2, 1), u32::MAX, "LoadAccessFault", 0),
            (
                load(5, 6, 2, 0),
                ADDRESS_SPACE_SIZE + 1,
                "LoadAddressMisaligned",
                ADDRESS_SPACE_SIZE + 1,
            ),
        ];

        for (instruction, base, cause, value) in cases {
            let code = [addi(8, 8, 1), instruction];
            let native = native_block(&code);
            let mut actual = machine_with_code(&code, IMAGE_START);
            actual.registers[5] = 0xfeed_face;
            actual.registers[6] = base;

            let outcome = execute_native(&native, &mut actual);
            actual.pc = outcome.next_pc();
            actual.retired += u64::from(outcome.retired());
            assert!(outcome.needs_interpreter());
            assert_eq!(outcome.next_pc(), IMAGE_START + 4);
            assert_eq!(outcome.retired(), 1);
            assert_eq!(actual.registers[5], 0xfeed_face);

            let fallback = actual.fetch_decode(actual.pc);
            let Some(Termination::Trap(trap)) = actual.execute_one(fallback) else {
                panic!("side-exit fallback must trap");
            };
            assert_eq!(trap.cause, cause);
            assert_eq!(trap.value, value);
            assert_eq!(actual.retired, 1);
            assert_eq!(actual.registers[5], 0xfeed_face);
        }

        let code = [store(7, 6, 2, 1)];
        let native = native_block(&code);
        let mut actual = machine_with_code(&code, IMAGE_START);
        actual.registers[6] = STACK_START;
        actual.registers[7] = 0xaabb_ccdd;
        actual
            .memory
            .store(STACK_START, 4, 0x4433_2211, IMAGE_START)
            .unwrap();
        let outcome = execute_native(&native, &mut actual);
        assert!(outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 0);
        assert_eq!(actual.memory.load_u32(STACK_START), 0x4433_2211);

        let code = [store(7, 6, 2, 0)];
        let native = native_block(&code);
        let mut actual = machine_with_code(&code, IMAGE_START);
        actual.registers[6] = IMAGE_START;
        actual.registers[7] = 0xaabb_ccdd;
        let original = actual.memory.load_u32(IMAGE_START);
        let outcome = execute_native(&native, &mut actual);
        assert!(outcome.needs_interpreter());
        assert_eq!(outcome.retired(), 0);
        assert_eq!(actual.memory.load_u32(IMAGE_START), original);
    }

    #[test]
    fn maximum_rv32_page_faults_safely_for_every_native_memory_width() {
        // These aligned addresses exercise the final padded permission byte.
        // A missing byte or a nonzero tail would permit an out-of-bounds access
        // relative to the 64 MiB flat data allocation.
        let load_cases = [
            (0, u32::MAX),
            (4, u32::MAX),
            (1, u32::MAX - 1),
            (5, u32::MAX - 1),
            (2, u32::MAX - 3),
        ];
        for (funct3, address) in load_cases {
            let code = [load(5, 6, funct3, 0)];
            let native = native_block(&code);
            let mut machine = machine_with_code(&code, IMAGE_START);
            machine.registers[5] = 0xfeed_face;
            machine.registers[6] = address;

            let outcome = execute_native(&native, &mut machine);

            assert_eq!(outcome.next_pc(), IMAGE_START);
            assert_eq!(outcome.retired(), 0);
            assert_eq!(machine.registers[5], 0xfeed_face);
            let trap = interpret_side_exit(&mut machine, outcome);
            assert_eq!(trap.cause, "LoadAccessFault");
            assert_eq!(trap.pc, IMAGE_START);
            assert_eq!(trap.value, address);
            assert_eq!(machine.registers[5], 0xfeed_face);
        }

        let store_cases = [(0, u32::MAX), (1, u32::MAX - 1), (2, u32::MAX - 3)];
        for (funct3, address) in store_cases {
            let code = [store(7, 6, funct3, 0)];
            let native = native_block(&code);
            let mut machine = machine_with_code(&code, IMAGE_START);
            machine.registers[6] = address;
            machine.registers[7] = 0xaabb_ccdd;
            machine
                .memory
                .store(STACK_START, 4, 0x4433_2211, IMAGE_START)
                .unwrap();

            let outcome = execute_native(&native, &mut machine);

            assert_eq!(outcome.next_pc(), IMAGE_START);
            assert_eq!(outcome.retired(), 0);
            assert_eq!(machine.memory.load_u32(STACK_START), 0x4433_2211);
            let trap = interpret_side_exit(&mut machine, outcome);
            assert_eq!(trap.cause, "StoreAccessFault");
            assert_eq!(trap.pc, IMAGE_START);
            assert_eq!(trap.value, address);
            assert_eq!(machine.memory.load_u32(STACK_START), 0x4433_2211);
        }
    }

    #[test]
    fn wrapping_memory_offsets_and_out_of_range_alignment_keep_exact_traps() {
        let wrapping_loads = [
            (load(5, 6, 0, -1), 0, u32::MAX),
            (load(5, 6, 1, -2), 0, u32::MAX - 1),
            (load(5, 6, 2, 1), u32::MAX, 0),
        ];
        for (instruction, base, address) in wrapping_loads {
            let code = [instruction];
            let native = native_block(&code);
            let mut machine = machine_with_code(&code, IMAGE_START);
            machine.registers[5] = 0xfeed_face;
            machine.registers[6] = base;

            let outcome = execute_native(&native, &mut machine);
            let trap = interpret_side_exit(&mut machine, outcome);

            assert_eq!(trap.cause, "LoadAccessFault");
            assert_eq!(trap.value, address);
            assert_eq!(machine.registers[5], 0xfeed_face);
        }

        let wrapping_stores = [
            (store(7, 6, 0, -1), 0, u32::MAX),
            (store(7, 6, 1, -2), 0, u32::MAX - 1),
            (store(7, 6, 2, 1), u32::MAX, 0),
        ];
        for (instruction, base, address) in wrapping_stores {
            let code = [instruction];
            let native = native_block(&code);
            let mut machine = machine_with_code(&code, IMAGE_START);
            machine.registers[6] = base;
            machine.registers[7] = 0xaabb_ccdd;

            let outcome = execute_native(&native, &mut machine);
            let trap = interpret_side_exit(&mut machine, outcome);

            assert_eq!(trap.cause, "StoreAccessFault");
            assert_eq!(trap.value, address);
        }

        // Alignment is architecturally checked before the now-combined
        // range/permission guard, even when the address is already outside the
        // 64 MiB data allocation.
        let precedence_cases = [
            (
                load(5, 6, 1, 0),
                ADDRESS_SPACE_SIZE + 1,
                "LoadAddressMisaligned",
            ),
            (
                load(5, 6, 2, 0),
                ADDRESS_SPACE_SIZE + 2,
                "LoadAddressMisaligned",
            ),
            (
                store(7, 6, 1, 0),
                ADDRESS_SPACE_SIZE + 1,
                "StoreAddressMisaligned",
            ),
            (
                store(7, 6, 2, 0),
                ADDRESS_SPACE_SIZE + 2,
                "StoreAddressMisaligned",
            ),
        ];
        for (instruction, address, cause) in precedence_cases {
            let code = [instruction];
            let native = native_block(&code);
            let mut machine = machine_with_code(&code, IMAGE_START);
            machine.registers[6] = address;
            machine.registers[7] = 0xaabb_ccdd;

            let outcome = execute_native(&native, &mut machine);
            let trap = interpret_side_exit(&mut machine, outcome);

            assert_eq!(trap.cause, cause);
            assert_eq!(trap.value, address);
        }
    }

    #[test]
    fn executes_jalr_and_clears_the_low_target_bit() {
        assert_matches_interpreter(&[jalr(5, 6, 0)], &[(6, IMAGE_START.wrapping_add(9))]);
        assert_matches_interpreter(&[jalr(6, 6, 0)], &[(6, IMAGE_START.wrapping_add(8))]);
    }

    #[test]
    fn misaligned_control_transfers_side_exit_before_link_mutation() {
        assert_matches_interpreter(&[branch(0, 6, 7, 2)], &[(6, 1), (7, 2)]);

        let cases = [
            (jalr(5, 6, 0), Some((6, IMAGE_START + 2))),
            (jal(5, 2), None),
            (branch(0, 6, 7, 2), Some((6, 9))),
            (branch(0, 0, 0, 2), None),
        ];

        for (instruction, setup) in cases {
            let code = [instruction];
            let native = native_block(&code);
            let mut actual = machine_with_code(&code, IMAGE_START);
            actual.registers[5] = 0xfeed_face;
            if let Some((register, value)) = setup {
                actual.registers[register] = value;
            }
            if instruction & 0x7f == 0x63 {
                actual.registers[7] = actual.registers[6];
            }

            let outcome = execute_native(&native, &mut actual);
            assert!(outcome.needs_interpreter());
            assert_eq!(outcome.next_pc(), IMAGE_START);
            assert_eq!(outcome.retired(), 0);
            assert_eq!(actual.registers[5], 0xfeed_face);

            let fallback = actual.fetch_decode(outcome.next_pc());
            let Some(Termination::Trap(trap)) = actual.execute_one(fallback) else {
                panic!("misaligned transfer must trap");
            };
            assert_eq!(trap.cause, "InstructionAddressMisaligned");
            assert_eq!(actual.retired, 0);
            assert_eq!(actual.registers[5], 0xfeed_face);
        }
    }

    #[test]
    fn cached_jalr_alias_spills_the_pre_transfer_value_on_side_exit() {
        let code = [addi(5, 5, 2), addi(5, 5, 2), jalr(5, 5, 2)];
        let native = native_block(&code);
        let mut actual = machine_with_code(&code, IMAGE_START);
        actual.registers[5] = IMAGE_START - 4;

        let outcome = execute_native(&native, &mut actual);
        assert!(outcome.needs_interpreter());
        assert_eq!(outcome.next_pc(), IMAGE_START + 8);
        assert_eq!(outcome.retired(), 2);
        assert_eq!(actual.registers[5], IMAGE_START);

        actual.pc = outcome.next_pc();
        actual.retired = u64::from(outcome.retired());
        let fallback = actual.fetch_decode(actual.pc);
        let Some(Termination::Trap(trap)) = actual.execute_one(fallback) else {
            panic!("misaligned cached JALR must trap");
        };
        assert_eq!(trap.cause, "InstructionAddressMisaligned");
        assert_eq!(actual.registers[5], IMAGE_START);
    }

    #[test]
    fn executes_all_branch_conditions() {
        let cases = [
            (0, (5, 5), (5, 6)),
            (1, (5, 6), (5, 5)),
            (4, (u32::MAX, 0), (0, u32::MAX)),
            (5, (0, u32::MAX), (u32::MAX, 0)),
            (6, (0, 1), (1, 0)),
            (7, (1, 0), (0, 1)),
        ];

        for (funct3, taken, not_taken) in cases {
            let code = [NOP, branch(funct3, 6, 7, 8)];
            assert_matches_interpreter(&code, &[(6, taken.0), (7, taken.1)]);
            assert_matches_interpreter(&code, &[(6, not_taken.0), (7, not_taken.1)]);
        }
    }

    #[test]
    fn cached_and_zero_branch_operands_match_the_interpreter() {
        let cases = [
            (0, (5, 5), (5, 6)),
            (1, (5, 6), (5, 5)),
            (4, (u32::MAX, 0), (0, u32::MAX)),
            (5, (0, u32::MAX), (u32::MAX, 0)),
            (6, (0, 1), (1, 0)),
            (7, (1, 0), (0, 1)),
        ];
        for (funct3, taken, not_taken) in cases {
            let cached = [
                addi(6, 6, 0),
                addi(6, 6, 0),
                addi(7, 7, 0),
                addi(7, 7, 0),
                branch(funct3, 6, 7, 8),
            ];
            assert_matches_interpreter(&cached, &[(6, taken.0), (7, taken.1)]);
            assert_matches_interpreter(&cached, &[(6, not_taken.0), (7, not_taken.1)]);

            let cached_canonical = [addi(6, 6, 0), addi(6, 6, 0), branch(funct3, 6, 8, 8)];
            assert_matches_interpreter(&cached_canonical, &[(6, taken.0), (8, taken.1)]);
            assert_matches_interpreter(&cached_canonical, &[(6, not_taken.0), (8, not_taken.1)]);

            let canonical_cached = [addi(6, 6, 0), addi(6, 6, 0), branch(funct3, 8, 6, 8)];
            assert_matches_interpreter(&canonical_cached, &[(8, taken.0), (6, taken.1)]);
            assert_matches_interpreter(&canonical_cached, &[(8, not_taken.0), (6, not_taken.1)]);
        }

        for funct3 in [0, 1, 4, 5, 6, 7] {
            for value in [0, 1, u32::MAX, 0x8000_0000] {
                let cached_right_zero = [addi(6, 6, 0), addi(6, 6, 0), branch(funct3, 6, 0, 8)];
                assert_matches_interpreter(&cached_right_zero, &[(6, value)]);

                let zero_left_cached = [addi(6, 6, 0), addi(6, 6, 0), branch(funct3, 0, 6, 8)];
                assert_matches_interpreter(&zero_left_cached, &[(6, value)]);
            }
            assert_matches_interpreter(&[branch(funct3, 0, 0, 8)], &[]);
        }
    }

    #[test]
    fn publishes_multiple_blocks_in_one_program() {
        let first_machine = machine_with_code(&[addi(5, 5, 1), NOP], IMAGE_START);
        let second_machine = machine_with_code(&[addi(6, 6, 2), NOP], IMAGE_START);
        let loop_machine = machine_with_code(&[addi(7, 7, 1), branch(4, 7, 8, -4)], IMAGE_START);
        let mut scalar_loop_code = vec![load(9, 10, 2, 0); 127];
        scalar_loop_code.push(jal(0, -508));
        let scalar_loop_machine = machine_with_code(&scalar_loop_code, IMAGE_START);
        let first = CompiledBlock::compile(&decoded_block(&first_machine, IMAGE_START)).unwrap();
        let second = CompiledBlock::compile(&decoded_block(&second_machine, IMAGE_START)).unwrap();
        let loop_instructions = decoded(&loop_machine, IMAGE_START, 2);
        let counted =
            CompiledBlock::compile_grouped_loop(&[RegionBlock::new(&loop_instructions)], 4)
                .unwrap();
        let scalar_loop_instructions = decoded(&scalar_loop_machine, IMAGE_START, 128);
        let scalar_counted =
            CompiledBlock::compile_loop(&[RegionBlock::new(&scalar_loop_instructions)]).unwrap();

        let program =
            NativeProgram::publish(vec![first, counted, scalar_counted, second], usize::MAX)
                .unwrap();
        let mut machine = machine_with_code(&[NOP], IMAGE_START);
        machine.registers[8] = 100;

        let memory = machine.memory.native_view();
        assert!(
            program
                .entry(0)
                .unwrap()
                .execute_with_limit(&mut machine.registers, memory, 1)
                .is_none()
        );
        assert_eq!(machine.registers[5], 0);
        let memory = machine.memory.native_view();
        assert_eq!(
            program
                .entry(0)
                .unwrap()
                .execute_with_limit(&mut machine.registers, memory, 2)
                .unwrap()
                .next_pc(),
            IMAGE_START + 8
        );
        let memory = machine.memory.native_view();
        assert!(
            program
                .entry(1)
                .unwrap()
                .execute_with_limit(&mut machine.registers, memory, 4)
                .is_none()
        );
        let memory = machine.memory.native_view();
        assert_eq!(
            program
                .entry(1)
                .unwrap()
                .execute_with_limit(&mut machine.registers, memory, 8)
                .unwrap()
                .retired(),
            8
        );
        let memory = machine.memory.native_view();
        assert_eq!(
            program
                .entry(3)
                .unwrap()
                .execute(&mut machine.registers, memory)
                .next_pc(),
            IMAGE_START + 8
        );
        assert_eq!(machine.registers[5], 1);
        assert_eq!(machine.registers[6], 2);
        assert_eq!(machine.registers[7], 4);

        let expected_metadata = [
            (NativeEntryKind::Bounded, 2, 2, 1),
            (NativeEntryKind::Loop, 2, 8, 4),
            (NativeEntryKind::Loop, 128, 128, 1),
            (NativeEntryKind::Bounded, 2, 2, 1),
        ];
        for (index, (kind, count, minimum, factor)) in expected_metadata.iter().copied().enumerate()
        {
            let entry = program.entry(index).unwrap();
            assert_eq!(entry.kind(), kind);
            assert_eq!(entry.instruction_count(), count);
            assert_eq!(entry.minimum_instruction_count(), minimum);
            assert_eq!(entry.loop_unroll_factor(), factor);
        }
        assert!(program.entry(expected_metadata.len()).is_none());
    }
}
