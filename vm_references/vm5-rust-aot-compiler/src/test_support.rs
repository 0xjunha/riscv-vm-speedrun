use rv32vm_rust_common::{
    machine::Machine,
    memory::{Image, PAGE_COUNT, PAGE_SHIFT, PAGE_SIZE, PERM_EXEC, PERM_READ},
};

pub(crate) fn addi(rd: u32, rs1: u32, immediate: i32) -> u32 {
    ((immediate as u32 & 0xfff) << 20) | (rs1 << 15) | (rd << 7) | 0x13
}

pub(crate) fn lw(rd: u32, rs1: u32, immediate: i32) -> u32 {
    ((immediate as u32 & 0xfff) << 20) | (rs1 << 15) | (2 << 12) | (rd << 7) | 0x03
}

pub(crate) fn beq(rs1: u32, rs2: u32, offset: i32) -> u32 {
    let immediate = offset as u32 & 0x1fff;
    ((immediate >> 12) << 31)
        | (((immediate >> 5) & 0x3f) << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (((immediate >> 1) & 0xf) << 8)
        | (((immediate >> 11) & 1) << 7)
        | 0x63
}

pub(crate) fn image_with_code_at(code: &[u32], start: u32) -> Image {
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

    Image {
        entry: start,
        permissions,
        pages,
        executable_file_ranges: std::iter::once(start..start + code.len() as u32 * 4).collect(),
    }
}

pub(crate) fn machine_with_code_at(code: &[u32], start: u32) -> Machine {
    Machine::new(&image_with_code_at(code, start), &[], 0)
}
