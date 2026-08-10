use rv32vm_rust_common::{
    machine::Machine,
    memory::{Image, PAGE_COUNT, PAGE_SHIFT, PAGE_SIZE, PERM_EXEC, PERM_READ},
};

use crate::BlockInstruction;

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
pub(crate) const NOP: u32 = 0x0000_0013;

pub(crate) fn addi(rd: u32, rs1: u32, immediate: i32) -> u32 {
    ((immediate as u32 & 0xfff) << 20) | (rs1 << 15) | (rd << 7) | 0x13
}

pub(crate) fn lw(rd: u32, rs1: u32, immediate: i32) -> u32 {
    ((immediate as u32 & 0xfff) << 20) | (rs1 << 15) | (2 << 12) | (rd << 7) | 0x03
}

pub(crate) fn machine_with_code(code: &[u32], start: u32) -> Machine {
    let mut permissions = vec![0; PAGE_COUNT];
    let mut pages = std::iter::repeat_with(|| None)
        .take(PAGE_COUNT)
        .collect::<Vec<_>>();

    for (index, instruction) in code.iter().enumerate() {
        let address = start + index as u32 * 4;
        let page_number = (address >> PAGE_SHIFT) as usize;
        permissions[page_number] = PERM_READ | PERM_EXEC;
        let page = pages[page_number].get_or_insert_with(|| Box::new([0; PAGE_SIZE]));
        let offset = address as usize & (PAGE_SIZE - 1);
        page[offset..offset + 4].copy_from_slice(&instruction.to_le_bytes());
    }

    Machine::new(
        &Image {
            entry: start,
            permissions,
            pages,
            executable_file_ranges: std::iter::once(start..start + code.len() as u32 * 4).collect(),
        },
        &[],
        0,
    )
}

pub(crate) fn decoded_block(machine: &Machine, start: u32) -> Vec<BlockInstruction> {
    let mut instructions = Vec::new();
    let mut pc = start;
    loop {
        let instruction = machine.fetch_decode(pc);
        let ends_block = instruction
            .as_ref()
            .map_or(true, |instruction| instruction.ends_block());
        instructions.push(instruction);
        if ends_block || instructions.len() == 64 {
            return instructions;
        }
        pc = pc.wrapping_add(4);
    }
}
