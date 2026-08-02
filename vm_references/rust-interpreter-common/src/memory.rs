use std::marker::PhantomData;
use std::ops::Range;

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
    /// Exact virtual ranges backed by executable bytes in the ELF.
    pub executable_file_ranges: Vec<Range<u32>>,
}

pub struct Memory {
    permissions: Vec<u8>,
    pages: Vec<Option<Page>>,
    page_addresses: Option<Box<[usize]>>,
}

/// A run-local, direct view of guest-memory metadata.
///
/// The pointers remain valid for the lifetime of this value. The exclusive
/// borrow carried by `_memory` prevents safe code from moving or mutating the
/// underlying [`Memory`] while a native runner is using them.
pub struct DirectMemory<'a> {
    permissions: *const u8,
    page_addresses: *const usize,
    _memory: PhantomData<&'a mut Memory>,
}

impl DirectMemory<'_> {
    /// Returns the base of the immutable, page-indexed permission table.
    pub const fn permissions_ptr(&self) -> *const u8 {
        self.permissions
    }

    /// Returns the base of the immutable, page-indexed page-address table.
    ///
    /// Each entry is either zero for a sparse page or the address of the
    /// corresponding resident page's first byte.
    pub const fn page_addresses_ptr(&self) -> *const usize {
        self.page_addresses
    }
}

impl Memory {
    pub fn new(image: &Image, input: &[u8]) -> Self {
        let mut memory = Self {
            permissions: image.permissions.clone(),
            pages: image.pages.clone(),
            page_addresses: None,
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

    /// Creates a direct view for one run, initializing its bounded address
    /// table on first use.
    pub fn direct_memory(&mut self) -> DirectMemory<'_> {
        if self.page_addresses.is_none() {
            let mut page_addresses = vec![0; PAGE_COUNT].into_boxed_slice();
            for (address, page) in page_addresses.iter_mut().zip(&mut self.pages) {
                *address = page.as_mut().map_or(0, |page| page.as_mut_ptr() as usize);
            }
            self.page_addresses = Some(page_addresses);
        }

        let page_addresses = self
            .page_addresses
            .as_ref()
            .expect("direct page-address table was initialized above");
        DirectMemory {
            permissions: self.permissions.as_ptr(),
            page_addresses: page_addresses.as_ptr(),
            _memory: PhantomData,
        }
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
        if let Some(page_addresses) = &mut self.page_addresses {
            page_addresses[page_number] = page.as_mut_ptr() as usize;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn image_with_resident_page(page_number: usize, permission: u8) -> Image {
        let mut permissions = vec![0; PAGE_COUNT];
        permissions[page_number] = permission;
        let mut pages = std::iter::repeat_with(|| None)
            .take(PAGE_COUNT)
            .collect::<Vec<_>>();
        let mut page = Box::new([0; PAGE_SIZE]);
        page[0] = 0xa5;
        pages[page_number] = Some(page);
        Image {
            entry: IMAGE_START,
            permissions,
            pages,
            executable_file_ranges: Vec::new(),
        }
    }

    #[test]
    fn direct_view_reports_permissions_and_resident_pages() {
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let sparse = resident + 1;
        let mut memory = Memory::new(
            &image_with_resident_page(resident, PERM_READ | PERM_EXEC),
            &[],
        );
        assert!(memory.page_addresses.is_none());

        let (permissions_ptr, page_addresses_ptr) = {
            let direct = memory.direct_memory();
            (direct.permissions_ptr(), direct.page_addresses_ptr())
        };

        let page_addresses = memory.page_addresses.as_ref().unwrap();
        assert_eq!(page_addresses.len(), PAGE_COUNT);
        assert_eq!(permissions_ptr, memory.permissions.as_ptr());
        assert_eq!(page_addresses_ptr, page_addresses.as_ptr());
        assert_eq!(memory.permissions[resident], PERM_READ | PERM_EXEC);
        assert_eq!(memory.permissions[sparse], 0);
        assert_eq!(page_addresses[sparse], 0);
        assert_eq!(
            page_addresses[resident],
            memory.pages[resident].as_ref().unwrap().as_ptr() as usize
        );
    }

    #[test]
    fn store_refreshes_the_direct_table_after_every_write() {
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let address = STACK_START + 8;
        let page_number = (address >> PAGE_SHIFT) as usize;
        let mut memory = Memory::new(&image_with_resident_page(resident, PERM_READ), &[]);
        {
            let _direct = memory.direct_memory();
        }
        assert_eq!(memory.page_addresses.as_ref().unwrap()[page_number], 0);

        memory.store(address, 4, 0x4433_2211, IMAGE_START).unwrap();
        let page_address = memory.pages[page_number].as_ref().unwrap().as_ptr() as usize;
        assert_eq!(
            memory.page_addresses.as_ref().unwrap()[page_number],
            page_address
        );
        assert_eq!(memory.load_u32(address), 0x4433_2211);

        // Replacing the derived entry models stale writable provenance and
        // proves that a later safe mutable page borrow always refreshes it.
        memory.page_addresses.as_mut().unwrap()[page_number] = 0;
        memory.store(address, 2, 0xbbaa, IMAGE_START).unwrap();
        assert_eq!(
            memory.page_addresses.as_ref().unwrap()[page_number],
            page_address
        );
        assert_eq!(memory.load_u16(address), 0xbbaa);
    }

    #[test]
    fn repeated_views_reuse_the_same_tables() {
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let mut memory = Memory::new(&image_with_resident_page(resident, PERM_READ), &[]);

        let (first_permissions, first_pages) = {
            let first = memory.direct_memory();
            (first.permissions_ptr(), first.page_addresses_ptr())
        };
        let second = memory.direct_memory();

        assert_eq!(second.permissions_ptr(), first_permissions);
        assert_eq!(second.page_addresses_ptr(), first_pages);
    }

    #[test]
    fn direct_tables_are_isolated_between_memory_instances() {
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let image = image_with_resident_page(resident, PERM_READ);
        let mut first_memory = Memory::new(&image, &[]);
        let mut second_memory = Memory::new(&image, &[]);

        let (first_permissions, first_pages) = {
            let first = first_memory.direct_memory();
            (first.permissions_ptr(), first.page_addresses_ptr())
        };
        let second = second_memory.direct_memory();

        assert_ne!(second.permissions_ptr(), first_permissions);
        assert_ne!(second.page_addresses_ptr(), first_pages);
    }
}
