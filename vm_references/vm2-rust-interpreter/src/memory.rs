use crate::error::GuestTrap;

pub const ADDRESS_SPACE_SIZE: u32 = 0x0400_0000;
pub const IMAGE_START: u32 = 0x0001_0000;
pub const IMAGE_END: u32 = 0x0300_0000;
pub const INPUT_START: u32 = 0x0300_0000;
pub const INPUT_END: u32 = 0x0340_0000;
pub const STACK_START: u32 = 0x0380_0000;
pub const STACK_END: u32 = 0x0400_0000;

pub const PAGE_SHIFT: u32 = 12;
pub const PAGE_SIZE: usize = 1 << PAGE_SHIFT;
pub const PAGE_COUNT: usize = ADDRESS_SPACE_SIZE as usize / PAGE_SIZE;

pub const PERM_READ: u8 = 1;
pub const PERM_WRITE: u8 = 2;
pub const PERM_EXEC: u8 = 4;

type Page = Box<[u8; PAGE_SIZE]>;

#[derive(Debug)]
pub struct Image {
    pub entry: u32,
    pub permissions: Vec<u8>,
    pub pages: Vec<Option<Page>>,
}

pub struct Memory {
    permissions: Vec<u8>,
    pages: Vec<Option<Page>>,
}

impl Memory {
    pub fn new(image: &Image, input: &[u8]) -> Self {
        let mut memory = Self {
            permissions: image.permissions.clone(),
            pages: image.pages.clone(),
        };

        let input_first = (INPUT_START >> PAGE_SHIFT) as usize;
        let input_last = (INPUT_END >> PAGE_SHIFT) as usize;
        memory.permissions[input_first..input_last].fill(PERM_READ);
        for (index, chunk) in input.chunks(PAGE_SIZE).enumerate() {
            if chunk.iter().any(|byte| *byte != 0) {
                let mut page = Box::new([0; PAGE_SIZE]);
                page[..chunk.len()].copy_from_slice(chunk);
                memory.pages[input_first + index] = Some(page);
            }
        }

        let stack_first = (STACK_START >> PAGE_SHIFT) as usize;
        let stack_last = (STACK_END >> PAGE_SHIFT) as usize;
        memory.permissions[stack_first..stack_last].fill(PERM_READ | PERM_WRITE);
        memory
    }

    pub fn check(
        &self,
        address: u32,
        size: u32,
        permission: u8,
        cause: &'static str,
        pc: u32,
    ) -> Result<(), GuestTrap> {
        let end = u64::from(address) + u64::from(size);
        if end > u64::from(ADDRESS_SPACE_SIZE) {
            return Err(GuestTrap::new(cause, pc, address));
        }
        if size == 0 {
            return Ok(());
        }
        let first = (address >> PAGE_SHIFT) as usize;
        let last = ((address + size - 1) >> PAGE_SHIFT) as usize;
        if self.permissions[first..=last]
            .iter()
            .any(|actual| actual & permission != permission)
        {
            return Err(GuestTrap::new(cause, pc, address));
        }
        Ok(())
    }

    pub fn load_u8(&self, address: u32) -> u8 {
        self.pages[(address >> PAGE_SHIFT) as usize]
            .as_ref()
            .map_or(0, |page| page[address as usize & (PAGE_SIZE - 1)])
    }

    pub fn load_u16(&self, address: u32) -> u16 {
        let page = &self.pages[(address >> PAGE_SHIFT) as usize];
        let offset = address as usize & (PAGE_SIZE - 1);
        page.as_ref().map_or(0, |bytes| {
            u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
        })
    }

    pub fn load_u32(&self, address: u32) -> u32 {
        let page = &self.pages[(address >> PAGE_SHIFT) as usize];
        let offset = address as usize & (PAGE_SIZE - 1);
        page.as_ref().map_or(0, |bytes| {
            u32::from_le_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ])
        })
    }

    pub fn read(&self, address: u32, size: u32) -> Vec<u8> {
        let mut result = Vec::with_capacity(size as usize);
        for offset in 0..size {
            result.push(self.load_u8(address + offset));
        }
        result
    }

    pub fn store(&mut self, address: u32, size: u32, value: u32, pc: u32) -> Result<(), GuestTrap> {
        self.check(address, size, PERM_WRITE, "StoreAccessFault", pc)?;
        let page_number = (address >> PAGE_SHIFT) as usize;
        let page = self.pages[page_number].get_or_insert_with(|| Box::new([0; PAGE_SIZE]));
        let offset = address as usize & (PAGE_SIZE - 1);
        let bytes = value.to_le_bytes();
        page[offset..offset + size as usize].copy_from_slice(&bytes[..size as usize]);
        Ok(())
    }

    pub fn inspect(&self, address: u32, size: u32) -> Result<Vec<u8>, String> {
        let end = u64::from(address) + u64::from(size);
        if end > u64::from(ADDRESS_SPACE_SIZE) {
            return Err("inspect range is outside guest address space".into());
        }
        if size != 0 {
            let first = (address >> PAGE_SHIFT) as usize;
            let last = ((address + size - 1) >> PAGE_SHIFT) as usize;
            if self.permissions[first..=last].contains(&0) {
                return Err("inspect range includes unmapped memory".into());
            }
        }
        Ok(self.read(address, size))
    }
}
