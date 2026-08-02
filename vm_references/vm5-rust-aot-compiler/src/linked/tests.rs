//! Native code-generation, linking, dispatch, memory, and ABI tests.

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
use rv32vm_rust_common::memory::{PAGE_SHIFT, PERM_READ, PERM_WRITE, STACK_START};
use rv32vm_rust_common::{
    machine::Machine,
    memory::{IMAGE_START, PAGE_COUNT},
};
use rv32vm_rust_x86_block_compiler::BlockInstruction;

use super::dispatch::{INSTRUCTIONS_PER_PAGE, MAX_DISPATCH_BYTES};
use super::executable_memory::mapping_length;
#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
use super::program::NativeStop;
use super::register_cache::{MAX_CACHED_REGISTERS, MIN_WEIGHTED_CACHE_ACCESSES};
use super::{
    BUDGET_VENEER_BYTES, BinaryOperation32, DispatchTable, EDGE_SLOT_BYTES, ENTRY_BYTES,
    EXIT_BUDGET, EXIT_INTERPRET_ONE, EXIT_MISSING, EXTERNAL_THUNK_BYTES, Emitter,
    INTERPRET_ONE_VENEER_BYTES, LinkedBlock, LinkedProgram, MAX_EXIT_TRAMPOLINE_BYTES,
    MAX_FIXED_CODE_BYTES, MAX_SHARED_PROLOGUE_BYTES, MISSING_VENEER_BYTES, MemoryWidth, Operand32,
    Register32, RegisterCache,
};
#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
use crate::test_support::beq;
use crate::test_support::{addi, image_with_code_at, jal, jalr, lw};

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
core::arch::global_asm!(
    r#"
    .text
    .globl vm5_cache6_abi_probe
    .type vm5_cache6_abi_probe,@function
vm5_cache6_abi_probe:
    push rbx
    push rbp
    push r12
    push r13
    push r14
    push r15
    push rdx
    mov rax, rdi
    mov rdi, rsi
    mov rbx, 0x1122334455667788
    mov rbp, 0x8877665544332211
    mov r12, 0x0123456789abcdef
    mov r13, 0xfedcba9876543210
    mov r14, 0x0f0e0d0c0b0a0908
    mov r15, 0x8070605040302010
    call rax
    mov rdx, [rsp]
    mov [rdx], rbx
    mov [rdx + 8], rbp
    mov [rdx + 16], r12
    mov [rdx + 24], r13
    mov [rdx + 32], r14
    mov [rdx + 40], r15
    add rsp, 8
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbp
    pop rbx
    ret
    .size vm5_cache6_abi_probe, .-vm5_cache6_abi_probe
    "#
);

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
unsafe extern "C" {
    fn vm5_cache6_abi_probe(
        entry: *const u8,
        context: *mut super::run_context::RunContext,
        output: *mut u64,
    );
}

fn decoded(machine: &Machine, start: u32, count: usize) -> Vec<BlockInstruction> {
    (0..count)
        .map(|index| machine.fetch_decode(start + index as u32 * 4))
        .collect()
}

fn block(machine: &Machine, start: u32, count: usize) -> LinkedBlock {
    LinkedBlock::compile(&decoded(machine, start, count)).unwrap()
}

fn relative_target(code: &[u8], displacement_offset: usize, instruction_end: usize) -> usize {
    let displacement = i32::from_le_bytes(
        code[displacement_offset..displacement_offset + 4]
            .try_into()
            .unwrap(),
    );
    usize::try_from(i64::try_from(instruction_end).unwrap() + i64::from(displacement)).unwrap()
}

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

fn store(rs1: u32, rs2: u32, funct3: u32, immediate: i32) -> u32 {
    let immediate = immediate as u32 & 0xfff;
    ((immediate >> 5) << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (funct3 << 12)
        | ((immediate & 0x1f) << 7)
        | 0x23
}

fn explicit_cache(guests: &[u8]) -> RegisterCache {
    RegisterCache::from_guests(guests)
}

#[test]
fn direct_operand_encodings_cover_rex_immediates_shifts_and_memory_widths() {
    let mut emitter = Emitter::new(RegisterCache::empty());

    emitter.emit_register_operand(
        BinaryOperation32::Add.opcode(),
        Register32::R12d,
        Operand32::Register(Register32::R13d),
    );
    assert_eq!(emitter.code, [0x45, 0x03, 0xe5]);

    emitter.code.clear();
    emitter.emit_register_operand(
        BinaryOperation32::Add.opcode(),
        Register32::Eax,
        Operand32::GuestMemory(124),
    );
    assert_eq!(emitter.code, [0x03, 0x46, 0x7c]);

    emitter.code.clear();
    emitter.emit_operand_register(&[0x89], Operand32::GuestMemory(20), Register32::R12d);
    assert_eq!(emitter.code, [0x44, 0x89, 0x66, 0x14]);

    let immediate_cases = [
        (
            u32::from_ne_bytes((-129_i32).to_ne_bytes()),
            &[0x81, 0xc5, 0x7f, 0xff, 0xff, 0xff][..],
        ),
        (
            u32::from_ne_bytes((-128_i32).to_ne_bytes()),
            &[0x83, 0xc5, 0x80][..],
        ),
        (127, &[0x83, 0xc5, 0x7f][..]),
        (128, &[0x81, 0xc5, 0x80, 0x00, 0x00, 0x00][..]),
        (255, &[0x81, 0xc5, 0xff, 0x00, 0x00, 0x00][..]),
    ];
    for (value, expected) in immediate_cases {
        emitter.code.clear();
        emitter.emit_group_immediate(0, Operand32::Register(Register32::Ebp), value);
        assert_eq!(emitter.code, expected);
    }

    emitter.code.clear();
    emitter.emit_shift_immediate(5, Operand32::Register(Register32::R15d), 31);
    assert_eq!(emitter.code, [0x41, 0xc1, 0xef, 0x1f]);
    emitter.code.clear();
    emitter.emit_shift_immediate(5, Operand32::Register(Register32::R15d), 1);
    assert_eq!(emitter.code, [0x41, 0xd1, 0xef]);

    let byte_stores = [
        (Register32::Ecx, &[0x41, 0x88, 0x0c, 0x01][..]),
        (Register32::Ebx, &[0x41, 0x88, 0x1c, 0x01][..]),
        (Register32::Ebp, &[0x41, 0x88, 0x2c, 0x01][..]),
        (Register32::R12d, &[0x45, 0x88, 0x24, 0x01][..]),
        (Register32::R13d, &[0x45, 0x88, 0x2c, 0x01][..]),
        (Register32::R14d, &[0x45, 0x88, 0x34, 0x01][..]),
        (Register32::R15d, &[0x45, 0x88, 0x3c, 0x01][..]),
    ];
    for (source, expected) in byte_stores {
        emitter.code.clear();
        emitter.emit_flat_store(source, MemoryWidth::Byte);
        assert_eq!(emitter.code, expected);
    }

    let half_stores = [
        (Register32::Ecx, &[0x66, 0x41, 0x89, 0x0c, 0x01][..]),
        (Register32::Ebx, &[0x66, 0x41, 0x89, 0x1c, 0x01][..]),
        (Register32::Ebp, &[0x66, 0x41, 0x89, 0x2c, 0x01][..]),
        (Register32::R12d, &[0x66, 0x45, 0x89, 0x24, 0x01][..]),
        (Register32::R13d, &[0x66, 0x45, 0x89, 0x2c, 0x01][..]),
        (Register32::R14d, &[0x66, 0x45, 0x89, 0x34, 0x01][..]),
        (Register32::R15d, &[0x66, 0x45, 0x89, 0x3c, 0x01][..]),
    ];
    let word_stores = [
        (Register32::Ecx, &[0x41, 0x89, 0x0c, 0x01][..]),
        (Register32::Ebx, &[0x41, 0x89, 0x1c, 0x01][..]),
        (Register32::Ebp, &[0x41, 0x89, 0x2c, 0x01][..]),
        (Register32::R12d, &[0x45, 0x89, 0x24, 0x01][..]),
        (Register32::R13d, &[0x45, 0x89, 0x2c, 0x01][..]),
        (Register32::R14d, &[0x45, 0x89, 0x34, 0x01][..]),
        (Register32::R15d, &[0x45, 0x89, 0x3c, 0x01][..]),
    ];
    for (width, cases) in [
        (MemoryWidth::Half, &half_stores[..]),
        (MemoryWidth::Word, &word_stores[..]),
    ] {
        for &(source, expected) in cases {
            emitter.code.clear();
            emitter.emit_flat_store(source, width);
            assert_eq!(emitter.code, expected);
        }
    }

    let load_destinations = [
        (Register32::Eax, 0x41, 0x04),
        (Register32::Ebx, 0x41, 0x1c),
        (Register32::Ebp, 0x41, 0x2c),
        (Register32::R12d, 0x45, 0x24),
        (Register32::R13d, 0x45, 0x2c),
        (Register32::R14d, 0x45, 0x34),
        (Register32::R15d, 0x45, 0x3c),
    ];
    let load_operations = [
        (MemoryWidth::Byte, true, &[0x0f, 0xbe][..]),
        (MemoryWidth::Byte, false, &[0x0f, 0xb6][..]),
        (MemoryWidth::Half, true, &[0x0f, 0xbf][..]),
        (MemoryWidth::Half, false, &[0x0f, 0xb7][..]),
        (MemoryWidth::Word, false, &[0x8b][..]),
    ];
    for (width, signed, opcode) in load_operations {
        for (destination, rex, modrm) in load_destinations {
            let mut expected = vec![rex];
            expected.extend_from_slice(opcode);
            expected.extend_from_slice(&[modrm, 0x01]);
            emitter.code.clear();
            emitter.emit_flat_load(destination, width, signed);
            assert_eq!(emitter.code, expected);
        }
    }
}

#[test]
fn flat_memory_validation_uses_full_rv32_permissions_and_exact_alignment() {
    for (width, permission, alignment_mask) in [
        (MemoryWidth::Byte, 1_u8, 0_u32),
        (MemoryWidth::Half, 2_u8, 1_u32),
        (MemoryWidth::Word, 1_u8, 3_u32),
    ] {
        let mut emitter = Emitter::new(RegisterCache::empty());
        let failures = emitter
            .checked_memory_address(0, 0, width, permission)
            .unwrap();

        let mut expected = vec![0x31, 0xc0]; // xor eax, eax
        if alignment_mask != 0 {
            expected.extend_from_slice(&[0xa8, alignment_mask as u8]); // test al, mask
            expected.extend_from_slice(&[0x0f, 0x85, 0, 0, 0, 0]); // jnz slow
        }
        expected.extend_from_slice(&[
            0x89, 0xc2, // mov edx, eax
            0xc1, 0xea, 0x0c, // shr edx, PAGE_SHIFT
            0x41, 0xf6, 0x04, 0x10, permission, // test [r8+rdx], permission
            0x0f, 0x84, 0, 0, 0, 0, // jz precise slow path
        ]);
        assert_eq!(emitter.code, expected);
        if alignment_mask == 0 {
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].displacement_offset, 14);
            assert_eq!(failures[0].instruction_end, 18);
        } else {
            assert_eq!(failures.len(), 2);
            assert_eq!(failures[0].displacement_offset, 6);
            assert_eq!(failures[0].instruction_end, 10);
            assert_eq!(failures[1].displacement_offset, 22);
            assert_eq!(failures[1].instruction_end, 26);
        }
    }
}

#[test]
fn private_support_boundary_matches_private_compilation() {
    let code = [
        upper_immediate(0x37, 5, 0x8123_4000),
        upper_immediate(0x17, 5, 0xffff_f000),
        jal(5, 8),
        branch(0, 5, 6, 8),
        branch(1, 5, 6, 8),
        branch(4, 5, 6, 8),
        branch(5, 5, 6, 8),
        branch(6, 5, 6, 8),
        branch(7, 5, 6, 8),
        addi(5, 6, -1),
        immediate(5, 6, 2, 0xfff),
        immediate(5, 6, 3, 0xfff),
        immediate(5, 6, 4, 0x55a),
        immediate(5, 6, 6, 0x055),
        immediate(5, 6, 7, 0x0ff),
        immediate(5, 6, 1, 31),
        immediate(5, 6, 5, 31),
        immediate(5, 6, 5, (0x20 << 5) | 31),
        register(5, 6, 7, 0, 0),
        register(5, 6, 7, 0, 0x20),
        register(5, 6, 7, 1, 0),
        register(5, 6, 7, 2, 0),
        register(5, 6, 7, 3, 0),
        register(5, 6, 7, 4, 0),
        register(5, 6, 7, 5, 0),
        register(5, 6, 7, 5, 0x20),
        register(5, 6, 7, 6, 0),
        register(5, 6, 7, 7, 0),
        register(5, 6, 7, 0, 1),
        register(5, 6, 7, 1, 1),
        register(5, 6, 7, 2, 1),
        register(5, 6, 7, 3, 1),
        register(5, 6, 7, 4, 1),
        register(5, 6, 7, 5, 1),
        register(5, 6, 7, 6, 1),
        register(5, 6, 7, 7, 1),
        0x0000_000f,
        (1 << 25) | (1 << 12) | (5 << 7) | 0x13,
        (2 << 25) | (7 << 20) | (6 << 15) | (5 << 7) | 0x33,
        (1 << 12) | 0x0f,
        (2 << 12) | 0x63,
        jal(5, 2),
        branch(0, 5, 6, 2),
        lw(6, 0, 0),
        0x0000_0073,
    ];
    let image = image_with_code_at(&code, IMAGE_START);
    let machine = Machine::new(&image, &[], 0);

    for index in 0..code.len() {
        let instruction = machine
            .fetch_decode(IMAGE_START + index as u32 * 4)
            .unwrap();
        let staged = LinkedBlock::compile(&[Ok(instruction)]).is_some();
        assert_eq!(staged, LinkedBlock::supports(instruction));
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
fn assert_one_matches_interpreter(instruction: u32, registers: &[(usize, u32)]) {
    let image = image_with_code_at(&[instruction], IMAGE_START);
    let mut expected = Machine::new(&image, &[], 0);
    let mut actual = Machine::new(&image, &[], 0);
    for &(register, value) in registers {
        expected.registers[register] = value;
        actual.registers[register] = value;
    }
    let staged = block(&expected, IMAGE_START, 1);
    let program = LinkedProgram::publish(vec![staged], usize::MAX).unwrap();

    let decoded = expected.fetch_decode(IMAGE_START);
    assert!(expected.execute_one(decoded).is_none());
    let native_run = program.entry(0).unwrap().execute(
        &mut actual.registers,
        &mut actual.memory,
        IMAGE_START,
        1,
    );

    assert_eq!(native_run.retired, 1);
    assert_eq!(native_run.pc, expected.pc);
    assert_eq!(actual.registers, expected.registers);
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
fn assert_first_with_selected_cache_matches_interpreter(
    instruction: u32,
    registers: &[(usize, u32)],
) {
    let code = vec![instruction; MIN_WEIGHTED_CACHE_ACCESSES as usize];
    let image = image_with_code_at(&code, IMAGE_START);
    let mut expected = Machine::new(&image, &[], 0);
    let mut actual = Machine::new(&image, &[], 0);
    for &(register, value) in registers {
        expected.registers[register] = value;
        actual.registers[register] = value;
    }
    let blocks = (0..code.len())
        .map(|index| block(&expected, IMAGE_START + index as u32 * 4, 1))
        .collect();
    let program = LinkedProgram::publish(blocks, usize::MAX).unwrap();
    assert!(program.cached_register_count() > 0);

    let decoded = expected.fetch_decode(IMAGE_START);
    assert!(expected.execute_one(decoded).is_none());
    let native = program.entry(0).unwrap().execute(
        &mut actual.registers,
        &mut actual.memory,
        IMAGE_START,
        1,
    );

    assert_eq!(native.retired, 1);
    assert_eq!(native.pc, expected.pc);
    assert_eq!(actual.registers, expected.registers);
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
fn publish_singletons(machine: &Machine, count: usize) -> LinkedProgram {
    LinkedProgram::publish(
        (0..count)
            .map(|index| block(machine, IMAGE_START + index as u32 * 4, 1))
            .collect(),
        usize::MAX,
    )
    .unwrap()
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn direct_cached_alu_immediate_and_branch_aliases_match_the_interpreter() {
    let binary_operations = [(0, 0), (0, 0x20), (4, 0), (6, 0), (7, 0), (0, 1)];
    for (funct3, funct7) in binary_operations {
        for instruction in [
            register(5, 5, 6, funct3, funct7),
            register(6, 5, 6, funct3, funct7),
            register(5, 5, 5, funct3, funct7),
            register(5, 0, 6, funct3, funct7),
            register(5, 6, 0, funct3, funct7),
        ] {
            assert_first_with_selected_cache_matches_interpreter(
                instruction,
                &[(5, 0x8000_0001), (6, 0xffff_fffd)],
            );
        }
    }

    let immediate_cases = [
        addi(5, 5, -128),
        addi(5, 6, 127),
        addi(5, 5, 128),
        immediate(5, 5, 4, 0x55a),
        immediate(5, 6, 6, 0x055),
        immediate(5, 5, 7, 0x0ff),
        immediate(5, 5, 1, 0),
        immediate(5, 5, 1, 31),
        immediate(5, 5, 5, 31),
        immediate(5, 5, 5, (0x20 << 5) | 31),
    ];
    for instruction in immediate_cases {
        assert_first_with_selected_cache_matches_interpreter(
            instruction,
            &[(5, 0x8000_0001), (6, 0x7fff_fffe)],
        );
    }

    let branch_operands = [
        (5, 6, 0x8000_0000, 1),
        (0, 5, 0, 1),
        (5, 0, u32::MAX, 0),
        (5, 5, 0x1234_5678, 0x1234_5678),
    ];
    for funct3 in [0, 1, 4, 5, 6, 7] {
        for (left, right, left_value, right_value) in branch_operands {
            let mut initial = Vec::new();
            if left != 0 {
                initial.push((left as usize, left_value));
            }
            if right != 0 {
                initial.push((right as usize, right_value));
            }
            assert_first_with_selected_cache_matches_interpreter(
                branch(funct3, left, right, 8),
                &initial,
            );
        }
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn direct_cached_load_destination_preserves_aliases_sparse_pages_and_failures() {
    let resident_cases = [
        (0, 1, 0x80, 0xffff_ff80),
        (4, 1, 0x80, 0x80),
        (1, 2, 0x8001, 0xffff_8001),
        (5, 2, 0x8001, 0x8001),
        (2, 4, 0x89ab_cdef, 0x89ab_cdef),
    ];
    for (funct3, width, value, result) in resident_cases {
        let instruction = load(5, 5, funct3, 0);
        let code = vec![instruction; MIN_WEIGHTED_CACHE_ACCESSES as usize];
        let image = image_with_code_at(&code, IMAGE_START);
        let mut expected = Machine::new(&image, &[], 0);
        let mut actual = Machine::new(&image, &[], 0);
        let address = STACK_START + 0x400;
        expected.registers[5] = address;
        actual.registers[5] = address;
        expected
            .memory
            .store(address, width, value, IMAGE_START)
            .unwrap();
        actual
            .memory
            .store(address, width, value, IMAGE_START)
            .unwrap();
        let program = publish_singletons(&expected, code.len());
        assert_eq!(program.cache.host(5), Some(super::CachedHost::Ebx));

        assert!(
            expected
                .execute_one(expected.fetch_decode(IMAGE_START))
                .is_none()
        );
        let native = program.entry(0).unwrap().execute(
            &mut actual.registers,
            &mut actual.memory,
            IMAGE_START,
            1,
        );
        assert_eq!(native.retired, 1);
        assert_eq!(native.pc, expected.pc);
        assert_eq!(actual.registers[5], result);
        assert_eq!(actual.registers, expected.registers);
        #[cfg(feature = "profile")]
        {
            assert_eq!(native.profile.direct_memory_load, 1);
            assert_eq!(native.profile.direct_memory_store, 0);
        }
    }

    let instruction = lw(5, 5, 0);
    let code = vec![instruction; MIN_WEIGHTED_CACHE_ACCESSES as usize];
    let sparse_address = 0x0100_0000;
    let mut sparse_image = image_with_code_at(&code, IMAGE_START);
    sparse_image.permissions[(sparse_address >> PAGE_SHIFT) as usize] = PERM_READ;
    let mut sparse = Machine::new(&sparse_image, &[], 0);
    sparse.registers[5] = sparse_address;
    let sparse_program = publish_singletons(&sparse, code.len());
    let sparse_run = sparse_program.entry(0).unwrap().execute(
        &mut sparse.registers,
        &mut sparse.memory,
        IMAGE_START,
        1,
    );
    assert_eq!(sparse_run.retired, 1);
    assert_eq!(sparse.registers[5], 0);

    for address in [0x0200_0000, STACK_START + 0x401] {
        let image = image_with_code_at(&code, IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[5] = address;
        let program = publish_singletons(&machine, code.len());
        let failed = program.entry(0).unwrap().execute(
            &mut machine.registers,
            &mut machine.memory,
            IMAGE_START,
            1,
        );
        assert_eq!(failed.stop, NativeStop::InterpretOne);
        assert_eq!(failed.retired, 0);
        assert_eq!(failed.pc, IMAGE_START);
        assert_eq!(machine.registers[5], address);
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn direct_cached_store_source_covers_bpl_widths_aliases_and_failures() {
    for (funct3, width, value) in [(0, 1, 0xa5), (1, 2, 0xbbaa), (2, 4, 0x4433_2211)] {
        let mut code = vec![addi(4, 4, 0); 6];
        let store_index = code.len();
        code.extend(vec![
            store(10, 5, funct3, 0);
            MIN_WEIGHTED_CACHE_ACCESSES as usize
        ]);
        let image = image_with_code_at(&code, IMAGE_START);
        let mut expected = Machine::new(&image, &[], 0);
        let mut actual = Machine::new(&image, &[], 0);
        let pc = IMAGE_START + store_index as u32 * 4;
        let address = STACK_START + 0x500;
        expected.pc = pc;
        expected.registers[5] = value;
        expected.registers[10] = address;
        actual.registers[5] = value;
        actual.registers[10] = address;
        expected.memory.store(address, 4, 0, pc).unwrap();
        actual.memory.store(address, 4, 0, pc).unwrap();
        let program = publish_singletons(&expected, code.len());
        assert_eq!(program.cache.host(5), Some(super::CachedHost::Ebp));

        assert!(expected.execute_one(expected.fetch_decode(pc)).is_none());
        let native = program.entry(store_index).unwrap().execute(
            &mut actual.registers,
            &mut actual.memory,
            pc,
            1,
        );
        assert_eq!(native.retired, 1);
        assert_eq!(native.pc, expected.pc);
        assert_eq!(actual.registers, expected.registers);
        assert_eq!(
            actual.memory.read(address, width),
            expected.memory.read(address, width)
        );
        #[cfg(feature = "profile")]
        {
            assert_eq!(native.profile.direct_memory_load, 0);
            assert_eq!(native.profile.direct_memory_store, 1);
        }
    }

    let alias_instruction = store(5, 5, 2, 0);
    let alias_code = vec![alias_instruction; MIN_WEIGHTED_CACHE_ACCESSES as usize];
    let alias_image = image_with_code_at(&alias_code, IMAGE_START);
    let mut alias = Machine::new(&alias_image, &[], 0);
    let alias_address = STACK_START + 0x600;
    alias.registers[5] = alias_address;
    alias
        .memory
        .store(alias_address, 4, 0, IMAGE_START)
        .unwrap();
    let alias_program = publish_singletons(&alias, alias_code.len());
    let alias_run = alias_program.entry(0).unwrap().execute(
        &mut alias.registers,
        &mut alias.memory,
        IMAGE_START,
        1,
    );
    assert_eq!(alias_run.retired, 1);
    assert_eq!(
        alias.memory.read(alias_address, 4),
        alias_address.to_le_bytes()
    );

    for address in [0x0200_0000, STACK_START + 0x501] {
        let code = vec![store(10, 5, 1, 0); MIN_WEIGHTED_CACHE_ACCESSES as usize];
        let image = image_with_code_at(&code, IMAGE_START);
        let mut machine = Machine::new(&image, &[], 0);
        machine.registers[5] = 0xbbaa;
        machine.registers[10] = address;
        let program = publish_singletons(&machine, code.len());
        let before = machine.registers;
        let failed = program.entry(0).unwrap().execute(
            &mut machine.registers,
            &mut machine.memory,
            IMAGE_START,
            1,
        );
        assert_eq!(failed.stop, NativeStop::InterpretOne);
        assert_eq!(failed.retired, 0);
        assert_eq!(machine.registers, before);
    }

    let sparse_address = 0x0100_0000;
    let sparse_code = vec![store(10, 5, 2, 0); MIN_WEIGHTED_CACHE_ACCESSES as usize];
    let mut sparse_image = image_with_code_at(&sparse_code, IMAGE_START);
    sparse_image.permissions[(sparse_address >> PAGE_SHIFT) as usize] = PERM_WRITE;
    let mut sparse = Machine::new(&sparse_image, &[], 0);
    sparse.registers[5] = 0xaabb_ccdd;
    sparse.registers[10] = sparse_address;
    let sparse_program = publish_singletons(&sparse, sparse_code.len());
    assert!(sparse_program.cache.host(5).is_some());
    let before = sparse.registers;
    let native = sparse_program.entry(0).unwrap().execute(
        &mut sparse.registers,
        &mut sparse.memory,
        IMAGE_START,
        1,
    );
    assert_eq!(native.stop, NativeStop::Budget);
    assert_eq!(native.retired, 1);
    assert_eq!(native.pc, IMAGE_START + 4);
    assert_eq!(sparse.registers, before);
    #[cfg(feature = "profile")]
    assert_eq!(native.profile.direct_memory_store, 1);
    assert_eq!(
        sparse.memory.read(sparse_address, 4),
        0xaabb_ccdd_u32.to_le_bytes()
    );
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn flat_sparse_store_is_visible_to_fallback_and_the_next_native_run() {
    let code = [store(10, 5, 2, 0), load(6, 10, 2, 0)];
    let address = 0x0100_0000;
    let mut image = image_with_code_at(&code, IMAGE_START);
    image.permissions[(address >> PAGE_SHIFT) as usize] = PERM_READ | PERM_WRITE;
    let mut machine = Machine::new(&image, &[], 0);
    machine.registers[5] = 0x4433_2211;
    machine.registers[10] = address;
    let blocks = vec![
        block(&machine, IMAGE_START, 1),
        block(&machine, IMAGE_START + 4, 1),
    ];
    let program = LinkedProgram::publish(blocks, usize::MAX).unwrap();

    let stored = program.entry(0).unwrap().execute(
        &mut machine.registers,
        &mut machine.memory,
        IMAGE_START,
        1,
    );
    assert_eq!(stored.stop, NativeStop::Budget);
    assert_eq!(stored.retired, 1);
    assert_eq!(stored.pc, IMAGE_START + 4);
    assert_eq!(
        machine.memory.read(address, 4),
        0x4433_2211_u32.to_le_bytes()
    );

    machine.pc = IMAGE_START + 4;
    assert!(
        machine
            .execute_one(machine.fetch_decode(IMAGE_START + 4))
            .is_none()
    );
    assert_eq!(machine.registers[6], 0x4433_2211);

    machine.registers[6] = 0;
    let loaded = program.entry(1).unwrap().execute(
        &mut machine.registers,
        &mut machine.memory,
        IMAGE_START + 4,
        1,
    );
    assert_eq!(loaded.retired, 1);
    assert_eq!(loaded.pc, IMAGE_START + 8);
    assert_eq!(machine.registers[6], 0x4433_2211);
}

#[cfg(all(
    feature = "profile",
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn direct_operand_profile_counts_each_executed_lowering_family() {
    let code = [addi(5, 5, 1), register(6, 5, 6, 0, 0), branch(0, 5, 6, 4)];
    let image = image_with_code_at(&code, IMAGE_START);
    let mut machine = Machine::new(&image, &[], 0);
    machine.registers[5] = 1;
    machine.registers[6] = 2;
    let program =
        LinkedProgram::publish(vec![block(&machine, IMAGE_START, 3)], usize::MAX).unwrap();

    let run = program.entry(0).unwrap().execute(
        &mut machine.registers,
        &mut machine.memory,
        IMAGE_START,
        3,
    );

    assert_eq!(run.profile.direct_immediate, 1);
    assert_eq!(run.profile.direct_register, 1);
    assert_eq!(run.profile.direct_branch, 1);
    assert_eq!(run.profile.direct_memory_load, 0);
    assert_eq!(run.profile.direct_memory_store, 0);
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
struct Rv32mHarness {
    machine: Machine,
    initial_registers: [u32; 32],
    program: LinkedProgram,
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
impl Rv32mHarness {
    fn new() -> Self {
        let code = (0..8)
            .map(|funct3| register(5, 6, 7, funct3, 1))
            .collect::<Vec<_>>();
        let image = image_with_code_at(&code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let blocks = (0..code.len())
            .map(|index| block(&machine, IMAGE_START + index as u32 * 4, 1))
            .collect();
        let program = LinkedProgram::publish(blocks, usize::MAX).unwrap();
        let initial_registers = machine.registers;
        Self {
            machine,
            initial_registers,
            program,
        }
    }

    fn assert_case(&mut self, funct3: usize, left: u32, right: u32) {
        let pc = IMAGE_START + funct3 as u32 * 4;
        self.machine.registers = self.initial_registers;
        self.machine.registers[6] = left;
        self.machine.registers[7] = right;
        self.machine.pc = pc;
        self.machine.retired = 0;
        let mut actual_registers = self.machine.registers;

        let decoded = self.machine.fetch_decode(pc);
        assert!(self.machine.execute_one(decoded).is_none());
        let actual = self.program.entry(funct3).unwrap().execute(
            &mut actual_registers,
            &mut self.machine.memory,
            pc,
            1,
        );

        assert_eq!(actual.retired, 1);
        assert_eq!(actual.pc, self.machine.pc);
        assert_eq!(
            actual_registers, self.machine.registers,
            "RV32M funct3={funct3}, left={left:#010x}, right={right:#010x}"
        );
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn rv32m_exhaustive_edge_value_table_matches_the_interpreter() {
    const VALUES: [u32; 17] = [
        0,
        1,
        2,
        3,
        0x0000_ffff,
        0x0001_0000,
        0x3fff_ffff,
        0x4000_0000,
        0x7fff_fffe,
        0x7fff_ffff,
        0x8000_0000,
        0x8000_0001,
        0xbfff_ffff,
        0xffff_0000,
        0xffff_fffd,
        0xffff_fffe,
        0xffff_ffff,
    ];
    let mut harness = Rv32mHarness::new();

    for funct3 in 0..8 {
        for left in VALUES {
            for right in VALUES {
                harness.assert_case(funct3, left, right);
            }
        }
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn rv32m_deterministic_randomized_differential() {
    fn next(state: &mut u64) -> u32 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        (*state >> 16) as u32
    }

    let mut harness = Rv32mHarness::new();
    let mut state = 0xd1b5_4a32_d192_ed03;
    for _ in 0..4_096 {
        let left = next(&mut state);
        let right = next(&mut state);
        for funct3 in 0..8 {
            harness.assert_case(funct3, left, right);
        }
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn rv32m_division_corner_cases_never_raise_host_divide_error() {
    let cases = [
        (4, 0x8000_0000, u32::MAX),
        (4, 0x8000_0000, 0),
        (4, 0x7fff_ffff, 0),
        (5, u32::MAX, 0),
        (6, 0x8000_0000, u32::MAX),
        (6, 0x8000_0000, 0),
        (7, u32::MAX, 0),
    ];
    for (funct3, left, right) in cases {
        assert_one_matches_interpreter(register(5, 6, 7, funct3, 1), &[(6, left), (7, right)]);
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn rv32m_high_products_cover_signed_and_unsigned_extremes() {
    let cases = [
        (1, 0x8000_0000, 0x8000_0000),
        (1, u32::MAX, u32::MAX),
        (2, 0x8000_0000, u32::MAX),
        (2, u32::MAX, 0x8000_0000),
        (3, 0x8000_0000, 0x8000_0000),
        (3, u32::MAX, u32::MAX),
    ];
    for (funct3, left, right) in cases {
        assert_one_matches_interpreter(register(5, 6, 7, funct3, 1), &[(6, left), (7, right)]);
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn rv32m_preserves_operand_aliases_and_x0() {
    for funct3 in 0..8 {
        assert_one_matches_interpreter(
            register(6, 6, 7, funct3, 1),
            &[(6, 0x8000_0001), (7, 0xffff_fffd)],
        );
        assert_one_matches_interpreter(register(7, 6, 7, funct3, 1), &[(6, 0x8000_0001), (7, 3)]);
        assert_one_matches_interpreter(register(6, 6, 6, funct3, 1), &[(6, 0x8000_0001)]);
        assert_one_matches_interpreter(register(0, 6, 7, funct3, 1), &[(6, 0x8000_0001), (7, 0)]);
        assert_one_matches_interpreter(register(5, 0, 7, funct3, 1), &[(7, 3)]);
        assert_one_matches_interpreter(register(5, 6, 0, funct3, 1), &[(6, 0x8000_0001)]);
    }
}

#[cfg(all(
    feature = "profile",
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn rv32m_generated_register_traffic_is_exact() {
    let code = (0..8)
        .map(|funct3| register(5, 6, 7, funct3, 1))
        .collect::<Vec<_>>();
    let image = image_with_code_at(&code, IMAGE_START);
    let mut machine = Machine::new(&image, &[], 0);
    let program =
        LinkedProgram::publish(vec![block(&machine, IMAGE_START, code.len())], usize::MAX).unwrap();
    let mut registers = machine.registers;
    registers[6] = 0x8000_0001;
    registers[7] = 3;

    let result = program.entry(0).unwrap().execute(
        &mut registers,
        &mut machine.memory,
        IMAGE_START,
        code.len() as u64,
    );

    assert_eq!(result.retired, code.len() as u64);
    assert_eq!(result.profile.blocks, 1);
    assert_eq!(result.profile.register_loads, 3);
    assert_eq!(result.profile.register_stores, 3);
    assert_eq!(result.profile.cache_fills, 3);
    assert_eq!(result.profile.cache_spills, 3);
    assert_eq!(result.profile.cache_read_hits, 16);
    assert_eq!(result.profile.cache_write_hits, 8);
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn every_lowering_matches_the_interpreter() {
    assert_one_matches_interpreter(upper_immediate(0x37, 5, 0x8123_4000), &[]);
    assert_one_matches_interpreter(upper_immediate(0x17, 5, 0xffff_f000), &[]);
    assert_one_matches_interpreter(0x0000_000f, &[]);
    assert_one_matches_interpreter(jal(5, 8), &[]);
    assert_one_matches_interpreter(jal(0, 8), &[]);

    let immediate_cases = [
        (addi(5, 6, -1), 0),
        (immediate(5, 6, 2, 0xfff), 0x8000_0000),
        (immediate(5, 6, 3, 0xfff), 0xffff_fffe),
        (immediate(5, 6, 4, 0x55a), 0xaa55_aa55),
        (immediate(5, 6, 6, 0x055), 0xaa00_aa00),
        (immediate(5, 6, 7, 0x0ff), 0xaa55_aa55),
        (immediate(5, 6, 1, 31), 1),
        (immediate(5, 6, 5, 31), 0x8000_0000),
        (immediate(5, 6, 5, (0x20 << 5) | 31), 0x8000_0000),
        (addi(0, 6, 1), 9),
    ];
    for (instruction, source) in immediate_cases {
        assert_one_matches_interpreter(instruction, &[(6, source)]);
    }

    let register_cases = [
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
    ];
    for (instruction, left, right) in register_cases {
        assert_one_matches_interpreter(instruction, &[(6, left), (7, right)]);
    }
    assert_one_matches_interpreter(register(5, 5, 5, 0, 0), &[(5, 0x8000_0001)]);
    assert_one_matches_interpreter(register(0, 6, 7, 0, 0), &[(6, 1), (7, 2)]);

    let branch_cases = [
        (0, (5, 5), (5, 6)),
        (1, (5, 6), (5, 5)),
        (4, (u32::MAX, 0), (0, u32::MAX)),
        (5, (0, u32::MAX), (u32::MAX, 0)),
        (6, (0, 1), (1, 0)),
        (7, (1, 0), (0, 1)),
    ];
    for (funct3, taken, not_taken) in branch_cases {
        let instruction = branch(funct3, 6, 7, 8);
        assert_one_matches_interpreter(instruction, &[(6, taken.0), (7, taken.1)]);
        assert_one_matches_interpreter(instruction, &[(6, not_taken.0), (7, not_taken.1)]);
    }
}

#[test]
fn mapping_budget_is_page_rounded() {
    assert_eq!(mapping_length(0, 4_096, usize::MAX), None);
    assert_eq!(mapping_length(1, 4_096, 4_095), None);
    assert_eq!(mapping_length(1, 4_096, 4_096), Some(4_096));
    assert_eq!(mapping_length(4_097, 4_096, 8_192), Some(8_192));
    assert_eq!(mapping_length(usize::MAX, 4_096, usize::MAX), None);
}

fn scoring_block(pc: u32, instructions: Vec<super::Lowering>) -> LinkedBlock {
    LinkedBlock {
        pc,
        instructions,
        flow: super::BlockFlow::Fallthrough {
            pc: pc.wrapping_add(4),
        },
        reserved_code_len: 0,
    }
}

fn scored_add(destination: usize, source: usize) -> super::Lowering {
    super::Lowering::Immediate {
        destination,
        source,
        operation: super::ImmediateOperation::Add(1),
    }
}

#[test]
fn register_cache_scoring_deduplicates_overlaps_and_weights_nested_loops() {
    let outside = scoring_block(96, vec![scored_add(1, 2)]);
    let inner = scoring_block(
        100,
        vec![
            scored_add(3, 4),
            scored_add(5, 6),
            super::Lowering::Branch {
                left: 0,
                right: 0,
                condition: super::Condition::Equal,
                fallthrough: 112,
                target: 104,
            },
        ],
    );
    let duplicate_inner = scoring_block(100, inner.instructions.clone());
    let outer = scoring_block(
        112,
        vec![super::Lowering::Branch {
            left: 0,
            right: 0,
            condition: super::Condition::Equal,
            fallthrough: 116,
            target: 100,
        }],
    );

    let scores = RegisterCache::scores(&[outside, inner, duplicate_inner, outer]);

    assert_eq!(scores[2], 2); // outside every loop: W=1
    assert_eq!(scores[4], 16); // outer loop only: W=8
    assert_eq!(scores[6], 30); // nested inner+outer loops: W=15
}

#[test]
fn register_cache_scoring_excludes_calls_forward_edges_and_x0() {
    let before = scoring_block(100, vec![scored_add(1, 2)]);
    let backward_call = scoring_block(
        104,
        vec![super::Lowering::Jump {
            destination: 1,
            link: 108,
            target: 100,
        }],
    );
    let forward_branch = scoring_block(
        108,
        vec![super::Lowering::Branch {
            left: 0,
            right: 0,
            condition: super::Condition::Equal,
            fallthrough: 112,
            target: 120,
        }],
    );

    let scores = RegisterCache::scores(&[before, backward_call, forward_branch]);

    assert_eq!(scores[0], 0);
    assert_eq!(scores[2], 2);
    assert_eq!(scores[1], 2); // one add write plus one call-link write
}

#[test]
fn register_cache_rejects_a_lone_write_and_a_lone_read() {
    let lone_write = scoring_block(
        0,
        vec![super::Lowering::WriteImmediate {
            destination: 5,
            value: 1,
        }],
    );
    let lone_read = scoring_block(
        4,
        vec![super::Lowering::IndirectJump {
            pc: 4,
            destination: 0,
            source: 6,
            immediate: 0,
            link: 8,
        }],
    );

    assert_eq!(RegisterCache::select(&[lone_write]).count(), 0);
    assert_eq!(RegisterCache::select(&[lone_read]).count(), 0);
}

#[test]
fn register_cache_requires_five_weighted_body_accesses() {
    let four = scoring_block(0, vec![scored_add(6, 5); 4]);
    let five = scoring_block(0, vec![scored_add(6, 5); 5]);

    assert_eq!(RegisterCache::select(&[four]).count(), 0);
    let cache = RegisterCache::select(&[five]);
    assert_eq!(cache.count(), 2);
    assert_eq!(cache.guests()[..2], [5, 6]);
}

#[test]
fn one_read_and_write_in_a_backward_loop_clear_the_cache_gate() {
    let loop_block = scoring_block(
        100,
        vec![
            scored_add(1, 2),
            super::Lowering::Branch {
                left: 0,
                right: 0,
                condition: super::Condition::Equal,
                fallthrough: 108,
                target: 100,
            },
        ],
    );

    let cache = RegisterCache::select(&[loop_block]);

    assert_eq!(cache.count(), 2);
    assert_eq!(cache.guests()[..2], [2, 1]);
}

#[test]
fn register_cache_selection_is_bounded_and_breaks_ties_by_guest_index() {
    let block = scoring_block(
        0,
        (1..=7)
            .flat_map(|destination| {
                (0..MIN_WEIGHTED_CACHE_ACCESSES).map(move |_| super::Lowering::WriteImmediate {
                    destination,
                    value: destination as u32,
                })
            })
            .collect(),
    );

    let cache = RegisterCache::select(&[block]);

    assert_eq!(cache.count(), MAX_CACHED_REGISTERS);
    assert_eq!(
        &cache.guests()[..MAX_CACHED_REGISTERS],
        &(1..=MAX_CACHED_REGISTERS as u8).collect::<Vec<_>>()
    );
    assert_eq!(RegisterCache::select(&[]).count(), 0);
}

#[cfg(not(feature = "profile"))]
#[test]
fn region_local_cache_selects_profitable_uncached_reuse_and_skips_jalr() {
    let instructions = vec![scored_add(7, 7); 3];
    let cache = explicit_cache(&[1, 2, 3, 4, 5, 6]);
    let flow = super::BlockFlow::Fallthrough { pc: 12 };
    let mut emitter = Emitter::new(cache);

    emitter.emit_block(&instructions, flow, 0).unwrap();

    assert_eq!(emitter.local_guest, Some(7));
    let fill = [0x44, 0x8b, 0x5e, super::register_offset(7)];
    let spill = [0x44, 0x89, 0x5e, super::register_offset(7)];
    assert_eq!(
        emitter
            .code
            .windows(fill.len())
            .filter(|bytes| *bytes == fill)
            .count(),
        1
    );
    assert_eq!(
        emitter
            .code
            .windows(spill.len())
            .filter(|bytes| *bytes == spill)
            .count(),
        1
    );

    emitter.select_local_cache(
        &instructions,
        super::BlockFlow::IndirectJump { target_hint: None },
    );
    assert_eq!(emitter.local_guest, None);
}

#[test]
fn register_cache_scoring_stays_bounded_at_the_full_block_limit() {
    let blocks = (0..super::MAX_LINKED_BLOCKS)
        .map(|index| {
            scoring_block(
                u32::try_from(index * 64 * 4).unwrap(),
                vec![scored_add(1, 2); 64],
            )
        })
        .collect::<Vec<_>>();

    let scores = RegisterCache::scores(&blocks);

    assert_eq!(scores[1], (super::MAX_LINKED_BLOCKS * 64) as u64);
    assert_eq!(scores[2], (super::MAX_LINKED_BLOCKS * 64 * 2) as u64);
}

#[test]
fn full_register_cache_entry_and_exit_match_the_fixed_maxima() {
    let block = scoring_block(
        IMAGE_START,
        (1..=MAX_CACHED_REGISTERS)
            .flat_map(|destination| {
                (0..MIN_WEIGHTED_CACHE_ACCESSES).map(move |_| super::Lowering::WriteImmediate {
                    destination,
                    value: destination as u32,
                })
            })
            .collect(),
    );
    let cache = RegisterCache::select(std::slice::from_ref(&block));
    let mut emitter = Emitter::new(cache);
    emitter
        .emit_block(&block.instructions, block.flow, block.pc)
        .unwrap();

    let resolved = emitter.resolve().unwrap();

    assert_eq!(resolved.shared_prologue_bytes, MAX_SHARED_PROLOGUE_BYTES);
    assert_eq!(resolved.exit_trampoline_bytes, MAX_EXIT_TRAMPOLINE_BYTES);
    assert_eq!(
        resolved.shared_prologue_bytes + resolved.exit_trampoline_bytes,
        MAX_FIXED_CODE_BYTES
    );
}

#[test]
fn zero_and_partial_cache_entry_exit_sizes_and_layout_are_exact() {
    let no_cache_block = scoring_block(
        IMAGE_START,
        vec![super::Lowering::WriteImmediate {
            destination: 1,
            value: 1,
        }],
    );
    let mut no_cache_emitter = Emitter::new(RegisterCache::empty());
    no_cache_emitter
        .emit_block(
            &no_cache_block.instructions,
            no_cache_block.flow,
            no_cache_block.pc,
        )
        .unwrap();
    let no_cache = no_cache_emitter.resolve().unwrap();

    let no_cache_entry = no_cache.entries[0].1;
    assert_eq!(no_cache_entry.external_offset, 0);
    assert_eq!(no_cache_entry.indirect_offset, 19);
    assert_eq!(no_cache_entry.hot_offset, 23);
    assert_eq!(&no_cache.code[..4], &ENTRY_BYTES);
    assert_eq!(
        &no_cache.code[4..19],
        &[
            0x48, 0x8b, 0x37, // mov rsi, [rdi]
            0x4c, 0x8b, 0x47, 0x18, // mov r8, [rdi+24]
            0x4c, 0x8b, 0x4f, 0x20, // mov r9, [rdi+32]
            0x4c, 0x8b, 0x57, 0x08, // mov r10, [rdi+8]
        ]
    );
    assert_eq!(&no_cache.code[19..23], &ENTRY_BYTES);

    let one_cache_block = scoring_block(
        IMAGE_START,
        vec![
            super::Lowering::WriteImmediate {
                destination: 1,
                value: 1,
            };
            MIN_WEIGHTED_CACHE_ACCESSES as usize
        ],
    );
    let one_cache = RegisterCache::select(std::slice::from_ref(&one_cache_block));
    assert_eq!(one_cache.count(), 1);
    let mut one_cache_emitter = Emitter::new(one_cache);
    one_cache_emitter
        .emit_block(
            &one_cache_block.instructions,
            one_cache_block.flow,
            one_cache_block.pc,
        )
        .unwrap();
    let one_cache = one_cache_emitter.resolve().unwrap();

    let profile_counter_bytes = if cfg!(feature = "profile") { 8 } else { 0 };
    assert_eq!(no_cache.external_thunk_bytes, 0);
    assert_eq!(no_cache.shared_prologue_bytes, 0);
    assert_eq!(no_cache.exit_trampoline_bytes, 33);
    assert_eq!(
        no_cache.hot_code_bytes + no_cache.cold_code_bytes,
        no_cache.code.len()
    );
    assert_eq!(one_cache.external_thunk_bytes, EXTERNAL_THUNK_BYTES);
    assert_eq!(one_cache.shared_prologue_bytes, 22 + profile_counter_bytes);
    assert_eq!(one_cache.exit_trampoline_bytes, 37 + profile_counter_bytes);
    assert!(one_cache.shared_prologue_bytes <= MAX_SHARED_PROLOGUE_BYTES);
    assert!(one_cache.exit_trampoline_bytes <= MAX_EXIT_TRAMPOLINE_BYTES);
}

#[test]
fn uncached_inline_entry_reservation_bounds_cached_and_uncached_images() {
    for words in [vec![addi(5, 5, 1)], vec![addi(5, 5, 1); 3]] {
        let image = image_with_code_at(&words, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        let block = block(&machine, IMAGE_START, words.len());
        let reserved = MAX_FIXED_CODE_BYTES + block.reserved_code_len();
        let cache = RegisterCache::select(std::slice::from_ref(&block));
        assert_eq!(cache.is_empty(), words.len() == 1);

        let mut emitter = Emitter::new(cache);
        emitter
            .emit_block(&block.instructions, block.flow, block.pc)
            .unwrap();
        let resolved = emitter.resolve().unwrap();

        assert!(resolved.code.len() <= reserved);
        if cache.is_empty() {
            assert_eq!(resolved.external_thunk_bytes, 0);
            assert_eq!(resolved.shared_prologue_bytes, 0);
            assert_eq!(
                resolved.code.len() + MAX_FIXED_CODE_BYTES - resolved.exit_trampoline_bytes,
                reserved
            );
        } else {
            assert_eq!(resolved.external_thunk_bytes, EXTERNAL_THUNK_BYTES);
            assert!(resolved.shared_prologue_bytes > 0);
        }
    }
}

#[test]
fn uncached_reservation_bounds_every_direct_family_for_all_cache_sizes() {
    let words = [
        upper_immediate(0x37, 5, 0x8123_4000),
        upper_immediate(0x17, 10, 0xffff_f000),
        addi(5, 5, -128),
        addi(6, 5, 128),
        immediate(7, 7, 4, 0x55a),
        immediate(8, 7, 6, 0x055),
        immediate(9, 9, 7, 0x0ff),
        immediate(10, 10, 1, 31),
        immediate(5, 5, 5, (0x20 << 5) | 31),
        register(5, 5, 6, 0, 0),
        register(6, 5, 6, 0, 0x20),
        register(7, 7, 8, 4, 0),
        register(8, 9, 8, 6, 0),
        register(9, 9, 10, 7, 0),
        register(10, 10, 5, 0, 1),
        load(5, 5, 0, 127),
        load(6, 6, 5, -128),
        load(10, 10, 2, 2_047),
        load(0, 5, 2, 0),
        load(5, 0, 0, 0),
        store(5, 6, 0, -128),
        store(7, 8, 1, 127),
        store(9, 10, 2, -2_048),
        store(5, 0, 2, 0),
        branch(0, 5, 6, 4),
        branch(1, 7, 0, 4),
        branch(4, 0, 8, 4),
        branch(5, 9, 10, 4),
        branch(6, 5, 0, 4),
        branch(7, 0, 6, 4),
    ];
    let image = image_with_code_at(&words, IMAGE_START);
    let machine = Machine::new(&image, &[], 0);
    let blocks = (0..words.len())
        .map(|index| block(&machine, IMAGE_START + index as u32 * 4, 1))
        .collect::<Vec<_>>();
    let reserved = blocks.iter().fold(MAX_FIXED_CODE_BYTES, |total, block| {
        total + block.reserved_code_len()
    });
    let guests = [5, 6, 7, 8, 9, 10];

    for count in 0..=guests.len() {
        let cache = explicit_cache(&guests[..count]);
        let mut emitter = Emitter::new(cache);
        for block in &blocks {
            emitter
                .emit_block(&block.instructions, block.flow, block.pc)
                .unwrap();
        }
        let resolved = emitter.resolve().unwrap();
        assert!(
            resolved.code.len() <= reserved,
            "cache size {count} emitted {} bytes beyond reservation {reserved}",
            resolved.code.len()
        );
    }

    fn visit_ordered_partial_mappings<F>(values: &mut [u8], index: usize, visit: &mut F)
    where
        F: FnMut(&[u8]),
    {
        visit(&values[..index]);
        if index == values.len() {
            return;
        }
        for candidate in index..values.len() {
            values.swap(index, candidate);
            visit_ordered_partial_mappings(values, index + 1, visit);
            values.swap(index, candidate);
        }
    }

    let mut permutation = guests;
    visit_ordered_partial_mappings(&mut permutation, 0, &mut |mapping| {
        let cache = explicit_cache(mapping);
        for block in &blocks {
            let mut emitter = Emitter::new(cache);
            emitter
                .emit_block(&block.instructions, block.flow, block.pc)
                .unwrap();
            let cached_reserved = emitter.reserved_code_len().unwrap();
            assert!(
                cached_reserved <= block.reserved_code_len(),
                "mapping {mapping:?} emitted {cached_reserved} reserved bytes for {:#x}, empty-cache reservation {}",
                block.pc,
                block.reserved_code_len()
            );
        }
    });
}

#[test]
fn every_external_and_indirect_entry_is_a_cet_landing_pad() {
    let code = [
        addi(5, 5, 1),
        addi(5, 5, 1),
        addi(5, 5, 1),
        addi(6, 6, 1),
        addi(6, 6, 1),
        addi(6, 6, 1),
    ];
    let image = image_with_code_at(&code, IMAGE_START);
    let machine = Machine::new(&image, &[], 0);
    let blocks = [
        block(&machine, IMAGE_START, 3),
        block(&machine, IMAGE_START + 12, 3),
    ];
    let cache = RegisterCache::select(&blocks);
    assert_eq!(cache.count(), 2);
    let mut emitter = super::Emitter::new(cache);
    for block in &blocks {
        emitter
            .emit_block(&block.instructions, block.flow, block.pc)
            .unwrap();
    }
    let hot_len = emitter.code.len();
    let resolved = emitter.resolve().unwrap();
    assert_eq!(resolved.hot_code_bytes, hot_len);
    assert_eq!(
        resolved.hot_code_bytes + resolved.cold_code_bytes,
        resolved.code.len()
    );
    let prologue = hot_len + resolved.external_thunk_bytes;
    for (_, entry) in &resolved.entries {
        assert!(entry.indirect_offset < hot_len);
        assert!(entry.external_offset >= hot_len);
        assert_eq!(
            &resolved.code[entry.external_offset..entry.external_offset + ENTRY_BYTES.len()],
            &ENTRY_BYTES
        );
        assert_eq!(
            &resolved.code[entry.indirect_offset..entry.hot_offset],
            &ENTRY_BYTES
        );
        assert_eq!(
            resolved.code[entry.external_offset + 4..entry.external_offset + 7],
            [0x4c, 0x8d, 0x1d]
        );
        assert_eq!(
            relative_target(
                &resolved.code,
                entry.external_offset + 7,
                entry.external_offset + 11,
            ),
            entry.indirect_offset
        );
        assert_eq!(resolved.code[entry.external_offset + 11], 0xe9);
        assert_eq!(
            relative_target(
                &resolved.code,
                entry.external_offset + 12,
                entry.external_offset + EXTERNAL_THUNK_BYTES,
            ),
            prologue
        );
    }
    assert_eq!(
        resolved.external_thunk_bytes,
        blocks.len() * EXTERNAL_THUNK_BYTES
    );
}

#[test]
fn adjacent_direct_edges_share_their_slots_with_cet_pads() {
    let code = [addi(5, 5, 1), addi(5, 5, 1), addi(5, 5, 1)];
    let image = image_with_code_at(&code, IMAGE_START);
    let machine = Machine::new(&image, &[], 0);
    let blocks = [
        block(&machine, IMAGE_START, 1),
        block(&machine, IMAGE_START + 4, 2),
    ];
    let cache = RegisterCache::select(&blocks);
    assert_eq!(cache.count(), 1);
    let mut emitter = Emitter::new(cache);
    for block in &blocks {
        emitter
            .emit_block(&block.instructions, block.flow, block.pc)
            .unwrap();
    }
    let first_edge = emitter.edges[0];
    let resolved = emitter.resolve().unwrap();
    let code = resolved.code;
    let entries = resolved.entries;
    let table = DispatchTable::build(&code, &entries).unwrap();

    assert_eq!(table.page_count(), 1);
    assert_eq!(table.entry_count(), 2);
    assert_eq!(
        table.bytes(),
        PAGE_COUNT * size_of::<usize>()
            + super::PAGE_SIZE
            + size_of::<Box<[u32; INSTRUCTIONS_PER_PAGE]>>()
    );
    for &(pc, entry) in &entries {
        assert_eq!(
            table.encoded_entry(pc),
            Some(u32::try_from(entry.indirect_offset).unwrap() + 1)
        );
        assert_eq!(
            &code[entry.indirect_offset..entry.indirect_offset + ENTRY_BYTES.len()],
            &ENTRY_BYTES
        );
    }
    assert_eq!(table.encoded_entry(IMAGE_START + 8), Some(0));

    let edge_end = first_edge.slot_offset + EDGE_SLOT_BYTES;
    assert_eq!(edge_end, entries[1].1.hot_offset);
    assert_eq!(entries[1].1.indirect_offset + ENTRY_BYTES.len(), edge_end);
    #[cfg(feature = "profile")]
    assert_eq!(
        &code[first_edge.slot_offset..edge_end],
        &[
            0x48,
            0xff,
            0x47,
            u8::try_from(super::PROFILE_DIRECT_LINKS_OFFSET).unwrap(),
            0x90,
            0xf3,
            0x0f,
            0x1e,
            0xfa,
        ]
    );
    #[cfg(not(feature = "profile"))]
    assert_eq!(
        &code[first_edge.slot_offset..edge_end],
        &[0x90, 0xf3, 0x0f, 0x1e, 0xfa]
    );
    assert_ne!(entries[1].1.hot_offset, entries[1].1.indirect_offset);
}

#[test]
fn dispatch_table_validates_keys_landings_and_sparse_page_bounds() {
    let mut code = vec![0; ENTRY_BYTES.len() * 2];
    code[..ENTRY_BYTES.len()].copy_from_slice(&ENTRY_BYTES);
    code[ENTRY_BYTES.len()..].copy_from_slice(&ENTRY_BYTES);
    let first = super::EntryMetadata {
        external_offset: 0,
        indirect_offset: 0,
        hot_offset: 0,
    };
    let second = super::EntryMetadata {
        external_offset: ENTRY_BYTES.len(),
        indirect_offset: ENTRY_BYTES.len(),
        hot_offset: ENTRY_BYTES.len(),
    };
    let next_page = IMAGE_START + super::PAGE_SIZE as u32;
    let entries = [(IMAGE_START, first), (next_page, second)];

    let table = DispatchTable::build(&code, &entries).unwrap();

    assert_eq!(table.page_count(), 2);
    assert_eq!(table.entry_count(), 2);
    assert_eq!(
        table.bytes(),
        PAGE_COUNT * size_of::<usize>()
            + 2 * super::PAGE_SIZE
            + 2 * size_of::<Box<[u32; INSTRUCTIONS_PER_PAGE]>>()
    );
    assert!(table.bytes() <= MAX_DISPATCH_BYTES);
    assert_eq!(
        MAX_DISPATCH_BYTES,
        PAGE_COUNT * size_of::<usize>()
            + super::MAX_LINKED_BLOCKS
                * (super::PAGE_SIZE + size_of::<Box<[u32; INSTRUCTIONS_PER_PAGE]>>())
    );
    assert_eq!(table.encoded_entry(IMAGE_START), Some(1));
    assert_eq!(
        table.encoded_entry(next_page),
        Some(u32::try_from(ENTRY_BYTES.len()).unwrap() + 1)
    );
    assert_eq!(table.encoded_entry(IMAGE_START + 4), Some(0));
    assert_eq!(
        table.encoded_entry(IMAGE_START + 2 * super::PAGE_SIZE as u32),
        Some(0)
    );

    assert!(DispatchTable::build(&code, &[(IMAGE_START, first), (IMAGE_START, second)]).is_none());
    assert!(DispatchTable::build(&code, &[(IMAGE_START + 2, first)]).is_none());
    assert!(DispatchTable::build(&code, &[(super::ADDRESS_SPACE_SIZE, first)]).is_none());

    let mut invalid_code = code.clone();
    invalid_code[first.indirect_offset] ^= 1;
    assert!(DispatchTable::build(&invalid_code, &[(IMAGE_START, first)]).is_none());
    let beyond_code = super::EntryMetadata {
        indirect_offset: code.len(),
        ..first
    };
    assert!(DispatchTable::build(&code, &[(IMAGE_START, beyond_code)]).is_none());
}

#[cfg(feature = "profile")]
#[test]
fn profile_context_offsets_use_disp32_beyond_the_signed_byte_range() {
    let mut emitter = Emitter::new(RegisterCache::empty());

    emitter.increment_context(127);
    emitter.increment_context(128);
    emitter.add_context(136, 1).unwrap();

    assert_eq!(
        emitter.code,
        [
            0x48, 0xff, 0x47, 0x7f, // inc qword ptr [rdi+127]
            0x48, 0xff, 0x87, 0x80, 0x00, 0x00, 0x00, // [rdi+128]
            0x48, 0x81, 0x87, 0x88, 0x00, 0x00, 0x00, // add [rdi+136]
            0x01, 0x00, 0x00, 0x00,
        ]
    );
}

#[test]
fn jalr_slow_paths_relocate_to_precise_and_shared_committed_veneers() {
    let image = image_with_code_at(&[jalr(5, 6, -8)], IMAGE_START);
    let machine = Machine::new(&image, &[], 0);
    let block = block(&machine, IMAGE_START, 1);
    let mut emitter = Emitter::new(RegisterCache::empty());
    emitter
        .emit_block(&block.instructions, block.flow, block.pc)
        .unwrap();
    let hot_len = emitter.code.len();
    let misaligned = emitter.interpret_one_exits[0].branches[0];
    let misses = emitter.indirect_misses.clone();

    let resolved = emitter.resolve().unwrap();
    let code = resolved.code;
    let exit_start = hot_len + resolved.external_thunk_bytes + resolved.shared_prologue_bytes;
    let interpret = exit_start + resolved.exit_trampoline_bytes + BUDGET_VENEER_BYTES;
    let dynamic_missing = interpret + INTERPRET_ONE_VENEER_BYTES;

    assert_eq!(
        relative_target(
            &code,
            misaligned.displacement_offset,
            misaligned.instruction_end,
        ),
        interpret
    );
    assert_eq!(&code[interpret..interpret + 4], &[0x49, 0x83, 0xc2, 1]);
    assert_eq!(code[interpret + 4], 0xb8);
    assert_eq!(
        u32::from_le_bytes(code[interpret + 5..interpret + 9].try_into().unwrap()),
        IMAGE_START
    );
    for miss in misses {
        assert_eq!(
            relative_target(&code, miss.displacement_offset, miss.instruction_end),
            dynamic_missing
        );
    }
    #[cfg(feature = "profile")]
    let missing_body = dynamic_missing + 4;
    #[cfg(not(feature = "profile"))]
    let missing_body = dynamic_missing;
    assert_eq!(&code[missing_body..missing_body + 2], &[0x89, 0xc8]);
    assert_eq!(code[missing_body + 2], 0xe9);
    assert_eq!(
        relative_target(&code, missing_body + 3, missing_body + 7),
        exit_start + 18
    );
}

#[test]
fn empty_publication_does_not_allocate_a_dispatch_root() {
    assert!(DispatchTable::build(&[], &[]).is_none());
    assert!(LinkedProgram::publish(Vec::new(), usize::MAX).is_none());
}

#[test]
fn valid_jalr_is_private_and_invalid_funct3_is_not() {
    let code = [jalr(5, 6, -1), jalr(5, 6, -1) | (1 << 12)];
    let image = image_with_code_at(&code, IMAGE_START);
    let machine = Machine::new(&image, &[], 0);
    let valid = machine.fetch_decode(IMAGE_START).unwrap();
    let invalid = machine.fetch_decode(IMAGE_START + 4).unwrap();

    assert!(LinkedBlock::supports(valid));
    assert!(LinkedBlock::ends_block(valid));
    assert!(!LinkedBlock::supports(invalid));
}

#[test]
fn compact_edges_and_cold_exits_have_exact_relocated_layout() {
    let code = [addi(5, 5, 1), addi(6, 6, 1)];
    let image = image_with_code_at(&code, IMAGE_START);
    let machine = Machine::new(&image, &[], 0);
    let blocks = [
        block(&machine, IMAGE_START, 1),
        block(&machine, IMAGE_START + 4, 1),
    ];
    let mut emitter = Emitter::new(RegisterCache::empty());
    for block in &blocks {
        emitter
            .emit_block(&block.instructions, block.flow, block.pc)
            .unwrap();
    }
    let hot_len = emitter.code.len();
    let first_budget = emitter.budget_exits[0];
    let first_edge = emitter.edges[0];
    let missing_edge = emitter.edges[1];
    let reserved_len = blocks.iter().fold(MAX_FIXED_CODE_BYTES, |total, block| {
        total + block.reserved_code_len()
    });

    let resolved = emitter.resolve().unwrap();
    let exit_start = hot_len + resolved.external_thunk_bytes + resolved.shared_prologue_bytes;
    let actual_fixed = resolved.shared_prologue_bytes + resolved.exit_trampoline_bytes;
    let exit_bytes = resolved.exit_trampoline_bytes;
    let code = resolved.code;
    let entries = resolved.entries;

    // The first edge links natively, so its conservative ten-byte missing
    // veneer reservation is absent from the finalized image.
    assert_eq!(
        code.len() + MISSING_VENEER_BYTES + (MAX_FIXED_CODE_BYTES - actual_fixed),
        reserved_len
    );
    #[cfg(not(feature = "profile"))]
    assert_eq!(EDGE_SLOT_BYTES, 5);
    #[cfg(feature = "profile")]
    assert_eq!(EDGE_SLOT_BYTES, 9);
    assert_eq!(BUDGET_VENEER_BYTES, 14);
    assert_eq!(exit_bytes, 33);
    assert_eq!(
        &code[exit_start..exit_start + 7],
        &[0xc7, 0x47, 0x14, 3, 0, 0, 0]
    );
    assert_eq!(
        &code[exit_start + 9..exit_start + 16],
        &[0xc7, 0x47, 0x14, 2, 0, 0, 0]
    );
    assert_eq!(&code[exit_start + 16..exit_start + 18], &[0xeb, 0x07]);
    assert_eq!(
        &code[exit_start + 18..exit_start + 25],
        &[0xc7, 0x47, 0x14, 1, 0, 0, 0]
    );
    assert_eq!(
        &code[exit_start + 25..exit_start + exit_bytes],
        &[0x4c, 0x89, 0x57, 0x08, 0x89, 0x47, 0x10, 0xc3]
    );

    let first_veneer = exit_start + exit_bytes;
    assert_eq!(
        relative_target(
            &code,
            first_budget.branch.displacement_offset,
            first_budget.branch.instruction_end,
        ),
        first_veneer
    );
    assert_eq!(
        &code[first_veneer..first_veneer + 4],
        &[0x49, 0x83, 0xc2, 1]
    );
    assert_eq!(code[first_veneer + 4], 0xb8);
    assert_eq!(
        u32::from_le_bytes(code[first_veneer + 5..first_veneer + 9].try_into().unwrap()),
        IMAGE_START
    );
    assert_eq!(code[first_veneer + 9], 0xe9);
    assert_eq!(
        relative_target(&code, first_veneer + 10, first_veneer + 14),
        exit_start + 9
    );

    #[cfg(feature = "profile")]
    let direct_jump = first_edge.slot_offset + 4;
    #[cfg(not(feature = "profile"))]
    let direct_jump = first_edge.slot_offset;
    #[cfg(feature = "profile")]
    assert_eq!(
        &code[first_edge.slot_offset..direct_jump],
        &[0x48, 0xff, 0x47, super::PROFILE_DIRECT_LINKS_OFFSET as u8,]
    );
    assert_eq!(code[direct_jump], 0xe9);
    assert_eq!(
        relative_target(&code, direct_jump + 1, direct_jump + 5),
        entries[1].1.hot_offset
    );

    let missing = missing_edge.slot_offset;
    assert_eq!(code[missing], 0xe9);
    let missing_veneer = exit_start + exit_bytes + blocks.len() * BUDGET_VENEER_BYTES;
    assert_eq!(
        relative_target(&code, missing + 1, missing + 5),
        missing_veneer
    );
    assert_eq!(code[missing_veneer], 0xb8);
    assert_eq!(
        u32::from_le_bytes(
            code[missing_veneer + 1..missing_veneer + 5]
                .try_into()
                .unwrap()
        ),
        IMAGE_START + 8
    );
    assert_eq!(code[missing_veneer + 5], 0xe9);
    assert_eq!(
        relative_target(
            &code,
            missing_veneer + 6,
            missing_veneer + MISSING_VENEER_BYTES
        ),
        exit_start + 18
    );
    assert_eq!(EXIT_MISSING, 1);
    assert_eq!(EXIT_BUDGET, 2);
    assert_eq!(EXIT_INTERPRET_ONE, 3);
}

#[test]
fn unresolved_edges_share_a_cold_veneer_by_guest_target() {
    // Offset four makes both successors name the same unavailable PC.
    let code = [branch(0, 5, 6, 4)];
    let image = image_with_code_at(&code, IMAGE_START);
    let machine = Machine::new(&image, &[], 0);
    let block = block(&machine, IMAGE_START, 1);
    let reserved_len = LinkedProgram::fixed_code_len() + block.reserved_code_len();
    let mut emitter = Emitter::new(RegisterCache::empty());
    emitter
        .emit_block(&block.instructions, block.flow, block.pc)
        .unwrap();
    let hot_len = emitter.code.len();
    #[cfg(feature = "profile")]
    let edge_offsets = [emitter.edges[0].slot_offset, emitter.edges[1].slot_offset];
    #[cfg(not(feature = "profile"))]
    let (jump_offset, conditional) = (
        emitter.edges[0].slot_offset,
        emitter.conditional_edges[0].branch,
    );

    let resolved = emitter.resolve().unwrap();
    let exit_start = hot_len + resolved.external_thunk_bytes + resolved.shared_prologue_bytes;
    let actual_fixed = resolved.shared_prologue_bytes + resolved.exit_trampoline_bytes;
    let exit_bytes = resolved.exit_trampoline_bytes;
    let code = resolved.code;

    // Admission reserves a veneer per edge, while final relocation emits
    // one veneer for this unique unresolved guest PC.
    assert_eq!(
        code.len() + MISSING_VENEER_BYTES + (MAX_FIXED_CODE_BYTES - actual_fixed),
        reserved_len
    );
    let veneer = exit_start + exit_bytes + BUDGET_VENEER_BYTES;
    #[cfg(feature = "profile")]
    for slot in edge_offsets {
        assert_eq!(code[slot], 0xe9);
        assert_eq!(relative_target(&code, slot + 1, slot + 5), veneer);
        assert_eq!(&code[slot + 5..slot + EDGE_SLOT_BYTES], &[0x90; 4]);
    }
    #[cfg(not(feature = "profile"))]
    {
        assert_eq!(code[jump_offset], 0xe9);
        assert_eq!(
            relative_target(&code, jump_offset + 1, jump_offset + 5),
            veneer
        );
        assert_eq!(code[conditional.instruction_end - 6], 0x0f);
        assert_eq!(
            relative_target(
                &code,
                conditional.displacement_offset,
                conditional.instruction_end,
            ),
            veneer
        );
    }
    assert_eq!(code[veneer], 0xb8);
    assert_eq!(
        u32::from_le_bytes(code[veneer + 1..veneer + 5].try_into().unwrap()),
        IMAGE_START + 4
    );
    assert_eq!(code[veneer + 5], 0xe9);
    assert_eq!(
        relative_target(&code, veneer + 6, veneer + MISSING_VENEER_BYTES),
        exit_start + 18
    );
}

#[test]
fn checked_memory_failures_relocate_to_one_cold_refund_veneer() {
    let code = [lw(5, 6, 0)];
    let image = image_with_code_at(&code, IMAGE_START);
    let machine = Machine::new(&image, &[], 0);
    let block = block(&machine, IMAGE_START, 1);
    let reserved_len = LinkedProgram::fixed_code_len() + block.reserved_code_len();
    let mut emitter = Emitter::new(RegisterCache::empty());
    emitter
        .emit_block(&block.instructions, block.flow, block.pc)
        .unwrap();
    let hot_len = emitter.code.len();
    let failures = emitter.interpret_one_exits[0].branches.clone();
    assert_eq!(failures.len(), 2);

    let resolved = emitter.resolve().unwrap();
    let exit_start = hot_len + resolved.external_thunk_bytes + resolved.shared_prologue_bytes;
    let actual_fixed = resolved.shared_prologue_bytes + resolved.exit_trampoline_bytes;
    let exit_bytes = resolved.exit_trampoline_bytes;
    let code = resolved.code;

    assert_eq!(
        code.len() + (MAX_FIXED_CODE_BYTES - actual_fixed),
        reserved_len
    );
    let veneer = exit_start + exit_bytes + BUDGET_VENEER_BYTES;
    for failure in failures {
        assert_eq!(
            relative_target(&code, failure.displacement_offset, failure.instruction_end),
            veneer
        );
    }
    assert_eq!(&code[veneer..veneer + 4], &[0x49, 0x83, 0xc2, 1]);
    assert_eq!(code[veneer + 4], 0xb8);
    assert_eq!(
        u32::from_le_bytes(code[veneer + 5..veneer + 9].try_into().unwrap()),
        IMAGE_START
    );
    assert_eq!(code[veneer + 9], 0xe9);
    assert_eq!(
        relative_target(&code, veneer + 10, veneer + INTERPRET_ONE_VENEER_BYTES),
        exit_start
    );
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn six_register_native_entry_restores_the_sysv_callee_saved_abi() {
    let code = (0..3)
        .flat_map(|_| (1..=6).map(|register| addi(register, register, 1)))
        .collect::<Vec<_>>();
    let image = image_with_code_at(&code, IMAGE_START);
    let mut machine = Machine::new(&image, &[], 0);
    let program =
        LinkedProgram::publish(vec![block(&machine, IMAGE_START, code.len())], usize::MAX).unwrap();
    assert_eq!(program.cached_register_count(), 6);
    let mut registers = [0_u32; 32];
    let direct_memory = machine.memory.direct_memory();
    let mut context = super::run_context::RunContext::new(
        registers.as_mut_ptr(),
        code.len() as u64,
        IMAGE_START,
        &direct_memory,
        program.dispatch.roots_ptr(),
        program.memory.address(),
    );
    let metadata = program.entries[0];
    // SAFETY: The finalized external offset is within the live RX mapping.
    let entry = unsafe { program.memory.address().add(metadata.external_offset) };
    let mut observed = [0_u64; 6];

    // SAFETY: The probe preserves its caller's ABI, passes the exact
    // RunContext ABI to the live generated entry, and writes six outputs.
    unsafe { vm5_cache6_abi_probe(entry, &mut context, observed.as_mut_ptr()) };

    assert_eq!(
        observed,
        [
            0x1122_3344_5566_7788,
            0x8877_6655_4433_2211,
            0x0123_4567_89ab_cdef,
            0xfedc_ba98_7654_3210,
            0x0f0e_0d0c_0b0a_0908,
            0x8070_6050_4030_2010,
        ]
    );
    assert_eq!(context.remaining, 0);
    assert_eq!(context.pc, IMAGE_START + code.len() as u32 * 4);
    assert_eq!(context.exit, EXIT_MISSING);
    assert_eq!(&registers[1..=6], &[3; 6]);
}

#[cfg(all(
    feature = "profile",
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn cache_profile_counts_entry_exit_hits_and_every_exit_class_exactly() {
    let image = image_with_code_at(&[addi(5, 5, 1); 3], IMAGE_START);
    let mut machine = Machine::new(&image, &[], 0);
    let program =
        LinkedProgram::publish(vec![block(&machine, IMAGE_START, 3)], usize::MAX).unwrap();
    assert_eq!(program.cached_guest_registers()[..1], [5]);

    let mut budget_registers = [0; 32];
    budget_registers[5] = 10;
    let budget = program.entry(0).unwrap().execute(
        &mut budget_registers,
        &mut machine.memory,
        IMAGE_START,
        0,
    );
    assert_eq!(budget.stop, NativeStop::Budget);
    assert_eq!(budget_registers[5], 10);
    assert_eq!(
        (
            budget.profile.register_loads,
            budget.profile.register_stores
        ),
        (1, 1)
    );
    assert_eq!(
        (budget.profile.cache_fills, budget.profile.cache_spills),
        (1, 1)
    );
    assert_eq!(
        (
            budget.profile.cache_read_hits,
            budget.profile.cache_write_hits
        ),
        (0, 0)
    );

    let mut missing_registers = [0; 32];
    missing_registers[5] = 20;
    let missing = program.entry(0).unwrap().execute(
        &mut missing_registers,
        &mut machine.memory,
        IMAGE_START,
        3,
    );
    assert_eq!(missing.stop, NativeStop::MissingSuccessor);
    assert_eq!(missing_registers[5], 23);
    assert_eq!(
        (
            missing.profile.register_loads,
            missing.profile.register_stores
        ),
        (1, 1)
    );
    assert_eq!(
        (missing.profile.cache_fills, missing.profile.cache_spills),
        (1, 1)
    );
    assert_eq!(
        (
            missing.profile.cache_read_hits,
            missing.profile.cache_write_hits
        ),
        (3, 3)
    );

    let load_image = image_with_code_at(&[lw(5, 6, 0)], IMAGE_START);
    let mut load_machine = Machine::new(&load_image, &[], 0);
    let load_program =
        LinkedProgram::publish(vec![block(&load_machine, IMAGE_START, 1)], usize::MAX).unwrap();
    assert_eq!(load_program.cached_register_count(), 0);
    let mut trap_registers = [0; 32];
    trap_registers[5] = 99;
    trap_registers[6] = 1;
    let interpret = load_program.entry(0).unwrap().execute(
        &mut trap_registers,
        &mut load_machine.memory,
        IMAGE_START,
        1,
    );
    assert_eq!(interpret.stop, NativeStop::InterpretOne);
    assert_eq!(interpret.retired, 0);
    assert_eq!((trap_registers[5], trap_registers[6]), (99, 1));
    assert_eq!(
        (
            interpret.profile.register_loads,
            interpret.profile.register_stores
        ),
        (1, 0)
    );
    assert_eq!(
        (
            interpret.profile.cache_fills,
            interpret.profile.cache_spills
        ),
        (0, 0)
    );
    assert_eq!(
        (
            interpret.profile.cache_read_hits,
            interpret.profile.cache_write_hits,
        ),
        (0, 0)
    );

    let jalr_image = image_with_code_at(&[jalr(5, 6, 0)], IMAGE_START);
    let mut jalr_machine = Machine::new(&jalr_image, &[], 0);
    let jalr_program =
        LinkedProgram::publish(vec![block(&jalr_machine, IMAGE_START, 1)], usize::MAX).unwrap();
    assert_eq!(jalr_program.cached_register_count(), 0);
    let mut jalr_registers = [0; 32];
    jalr_registers[6] = IMAGE_START + 0x100;
    let committed = jalr_program.entry(0).unwrap().execute(
        &mut jalr_registers,
        &mut jalr_machine.memory,
        IMAGE_START,
        1,
    );
    assert_eq!(committed.stop, NativeStop::MissingSuccessor);
    assert_eq!(committed.pc, IMAGE_START + 0x100);
    assert_eq!(jalr_registers[5], IMAGE_START + 4);
    assert_eq!(
        (
            committed.profile.register_loads,
            committed.profile.register_stores
        ),
        (1, 1)
    );
    assert_eq!(
        (
            committed.profile.cache_fills,
            committed.profile.cache_spills
        ),
        (0, 0)
    );
    assert_eq!(
        (
            committed.profile.cache_read_hits,
            committed.profile.cache_write_hits
        ),
        (0, 0)
    );
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn four_flat_accesses_use_inline_entries_across_repeated_short_runs() {
    let code = [addi(5, 5, 1), addi(5, 5, 1)];
    let image = image_with_code_at(&code, IMAGE_START);
    let mut machine = Machine::new(&image, &[], 0);
    let program = LinkedProgram::publish(
        vec![
            block(&machine, IMAGE_START, 1),
            block(&machine, IMAGE_START + 4, 1),
        ],
        usize::MAX,
    )
    .unwrap();
    assert_eq!(program.cached_register_count(), 0);
    for entry in &program.entries {
        assert_eq!(entry.indirect_offset, entry.external_offset + 19);
        assert_eq!(entry.hot_offset, entry.indirect_offset + ENTRY_BYTES.len());
    }
    #[cfg(feature = "profile")]
    {
        assert_eq!(program.external_thunk_bytes(), 0);
        assert_eq!(program.shared_prologue_bytes(), 0);
        assert!(program.hot_code_bytes() > 0);
        assert!(program.cold_code_bytes() > 0);
    }

    for initial in [0, 10] {
        let mut registers = [0; 32];
        registers[5] = initial;
        let result =
            program
                .entry(0)
                .unwrap()
                .execute(&mut registers, &mut machine.memory, IMAGE_START, 2);

        assert_eq!(result.pc, IMAGE_START + 8);
        assert_eq!(result.retired, 2);
        assert_eq!(result.stop, NativeStop::MissingSuccessor);
        assert_eq!(registers[5], initial + 2);
        #[cfg(feature = "profile")]
        {
            assert_eq!(result.profile.blocks, 2);
            assert_eq!(result.profile.direct_links, 1);
            assert_eq!(result.profile.register_loads, 2);
            assert_eq!(result.profile.register_stores, 2);
            assert_eq!(result.profile.cache_fills, 0);
            assert_eq!(result.profile.cache_spills, 0);
            assert_eq!(result.profile.cache_read_hits, 0);
            assert_eq!(result.profile.cache_write_hits, 0);
            assert_eq!(result.profile.fallthrough_blocks, 2);
            assert_eq!(result.profile.branch_blocks, 0);
            assert_eq!(result.profile.jump_blocks, 0);
        }
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn links_backward_branch_cycles_until_the_next_block_reservation() {
    let code = [addi(5, 5, 1), beq(0, 0, -4)];
    let image = image_with_code_at(&code, IMAGE_START);
    let mut machine = Machine::new(&image, &[], 0);
    let program = LinkedProgram::publish(
        vec![
            block(&machine, IMAGE_START, 1),
            block(&machine, IMAGE_START + 4, 1),
        ],
        usize::MAX,
    )
    .unwrap();
    let mut registers = [0; 32];

    let result =
        program
            .entry(0)
            .unwrap()
            .execute(&mut registers, &mut machine.memory, IMAGE_START, 4);

    assert_eq!(result.pc, IMAGE_START);
    assert_eq!(result.retired, 4);
    assert_eq!(result.stop, NativeStop::Budget);
    assert_eq!(registers[5], 2);
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn links_both_conditional_branch_successors() {
    let code = [beq(5, 0, 8), addi(6, 6, 1), addi(7, 7, 1)];
    let image = image_with_code_at(&code, IMAGE_START);
    let mut machine = Machine::new(&image, &[], 0);
    let program = LinkedProgram::publish(
        vec![
            block(&machine, IMAGE_START, 1),
            block(&machine, IMAGE_START + 4, 1),
            block(&machine, IMAGE_START + 8, 1),
        ],
        usize::MAX,
    )
    .unwrap();

    let mut taken = [0; 32];
    let taken_result =
        program
            .entry(0)
            .unwrap()
            .execute(&mut taken, &mut machine.memory, IMAGE_START, 2);
    assert_eq!(taken_result.pc, IMAGE_START + 12);
    assert_eq!(taken_result.retired, 2);
    assert_eq!(taken[6], 0);
    assert_eq!(taken[7], 1);

    let mut fallthrough = [0; 32];
    fallthrough[5] = 1;
    let fallthrough_result =
        program
            .entry(0)
            .unwrap()
            .execute(&mut fallthrough, &mut machine.memory, IMAGE_START, 3);
    assert_eq!(fallthrough_result.pc, IMAGE_START + 12);
    assert_eq!(fallthrough_result.retired, 3);
    assert_eq!(fallthrough[6], 1);
    assert_eq!(fallthrough[7], 1);
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn links_jal_and_commits_the_link_register() {
    let code = [jal(5, 8), addi(6, 6, 99), addi(7, 7, 1)];
    let image = image_with_code_at(&code, IMAGE_START);
    let mut machine = Machine::new(&image, &[], 0);
    let program = LinkedProgram::publish(
        vec![
            block(&machine, IMAGE_START, 1),
            block(&machine, IMAGE_START + 8, 1),
        ],
        usize::MAX,
    )
    .unwrap();
    let mut registers = [0; 32];

    let result =
        program
            .entry(0)
            .unwrap()
            .execute(&mut registers, &mut machine.memory, IMAGE_START, 2);

    assert_eq!(result.pc, IMAGE_START + 12);
    assert_eq!(result.retired, 2);
    assert_eq!(registers[5], IMAGE_START + 4);
    assert_eq!(registers[6], 0);
    assert_eq!(registers[7], 1);
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn short_budgets_change_no_guest_state() {
    let code = [addi(5, 5, 1), addi(5, 5, 1), addi(5, 5, 1)];
    let image = image_with_code_at(&code, IMAGE_START);
    let mut machine = Machine::new(&image, &[], 0);
    let program =
        LinkedProgram::publish(vec![block(&machine, IMAGE_START, 3)], usize::MAX).unwrap();

    for remaining in 0..3 {
        let mut registers = [0; 32];
        let result = program.entry(0).unwrap().execute(
            &mut registers,
            &mut machine.memory,
            IMAGE_START,
            remaining,
        );
        assert_eq!(result.pc, IMAGE_START);
        assert_eq!(result.retired, 0);
        assert_eq!(result.stop, NativeStop::Budget);
        assert_eq!(registers, [0; 32]);
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn maximum_block_budget_is_reserved_as_an_unsigned_count() {
    let code = vec![addi(5, 5, 1); 64];
    let image = image_with_code_at(&code, IMAGE_START);
    let mut machine = Machine::new(&image, &[], 0);
    let program =
        LinkedProgram::publish(vec![block(&machine, IMAGE_START, 64)], usize::MAX).unwrap();

    let mut short = [0; 32];
    let short_result =
        program
            .entry(0)
            .unwrap()
            .execute(&mut short, &mut machine.memory, IMAGE_START, 63);
    assert_eq!(short_result.pc, IMAGE_START);
    assert_eq!(short_result.retired, 0);
    assert_eq!(short_result.stop, NativeStop::Budget);
    assert_eq!(short[5], 0);

    let mut exact = [0; 32];
    let exact_result =
        program
            .entry(0)
            .unwrap()
            .execute(&mut exact, &mut machine.memory, IMAGE_START, 64);
    assert_eq!(exact_result.pc, IMAGE_START + 64 * 4);
    assert_eq!(exact_result.retired, 64);
    assert_eq!(exact_result.stop, NativeStop::MissingSuccessor);
    assert_eq!(exact[5], 64);

    let mut huge = [0; 32];
    let huge_result =
        program
            .entry(0)
            .unwrap()
            .execute(&mut huge, &mut machine.memory, IMAGE_START, u64::MAX);
    assert_eq!(huge_result.pc, IMAGE_START + 64 * 4);
    assert_eq!(huge_result.retired, 64);
    assert_eq!(huge_result.stop, NativeStop::MissingSuccessor);
    assert_eq!(huge[5], 64);
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn failed_successor_reservation_preserves_the_committed_prefix_budget() {
    let code = [addi(5, 5, 1), addi(6, 6, 1), addi(6, 6, 1), addi(6, 6, 1)];
    let image = image_with_code_at(&code, IMAGE_START);
    let mut machine = Machine::new(&image, &[], 0);
    let program = LinkedProgram::publish(
        vec![
            block(&machine, IMAGE_START, 1),
            block(&machine, IMAGE_START + 4, 3),
        ],
        usize::MAX,
    )
    .unwrap();
    let mut registers = [0; 32];

    let result =
        program
            .entry(0)
            .unwrap()
            .execute(&mut registers, &mut machine.memory, IMAGE_START, 2);

    assert_eq!(result.pc, IMAGE_START + 4);
    assert_eq!(result.retired, 1);
    assert_eq!(result.stop, NativeStop::Budget);
    assert_eq!(registers[5], 1);
    assert_eq!(registers[6], 0);
    #[cfg(feature = "profile")]
    {
        assert_eq!(result.profile.blocks, 1);
        assert_eq!(result.profile.direct_links, 1);
        assert_eq!(result.profile.fallthrough_blocks, 1);
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn repeated_invocations_have_run_local_budget_and_register_state() {
    let code = [addi(5, 5, 1), beq(0, 0, -4)];
    let image = image_with_code_at(&code, IMAGE_START);
    let mut machine = Machine::new(&image, &[], 0);
    let program =
        LinkedProgram::publish(vec![block(&machine, IMAGE_START, 2)], usize::MAX).unwrap();

    for expected in [2, 3] {
        let mut registers = [0; 32];
        registers[5] = expected - 1;
        let result =
            program
                .entry(0)
                .unwrap()
                .execute(&mut registers, &mut machine.memory, IMAGE_START, 2);
        assert_eq!(result.retired, 2);
        assert_eq!(registers[5], expected);
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
#[test]
fn publication_obeys_code_budget_and_drop_unmaps_rx_memory() {
    let code = [addi(5, 5, 1)];
    let image = image_with_code_at(&code, IMAGE_START);
    let machine = Machine::new(&image, &[], 0);
    assert!(LinkedProgram::publish(vec![block(&machine, IMAGE_START, 1)], 4_095).is_none());
    let program = LinkedProgram::publish(vec![block(&machine, IMAGE_START, 1)], 4_096).unwrap();
    let address = program.memory.address() as usize;
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    let line = maps
        .lines()
        .find(|line| {
            let range = line.split_whitespace().next().unwrap();
            let (start, end) = range.split_once('-').unwrap();
            let start = usize::from_str_radix(start, 16).unwrap();
            let end = usize::from_str_radix(end, 16).unwrap();
            start <= address && address < end
        })
        .unwrap();
    let permissions = line.split_whitespace().nth(1).unwrap();
    assert!(permissions.starts_with("r-x"), "{permissions}");
    assert!(!permissions.contains('w'), "{permissions}");

    let unmap_status = program.memory.unmap_status();
    drop(program);
    assert_eq!(
        unmap_status.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "LinkedProgram::drop failed to unmap its executable memory"
    );
}
