use std::{marker::PhantomData, ops::Range, sync::Arc};

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
/// Page-indexed permission entries covering every address representable by RV32.
pub const RV32_PAGE_COUNT: usize = 1_usize << (u32::BITS - PAGE_SHIFT);

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

/// Immutable permission backing shared by runs of one loaded image.
#[derive(Clone)]
pub(crate) struct PermissionTemplate {
    permissions: Arc<[u8]>,
}

impl PermissionTemplate {
    pub(crate) fn new(image: &Image) -> Self {
        // Copy only architectural permissions. The zero tail makes every
        // possible RV32 page lookup safe without granting out-of-range access.
        let mut permissions = vec![0; RV32_PAGE_COUNT];
        permissions[..PAGE_COUNT].copy_from_slice(&image.permissions[..PAGE_COUNT]);

        let input_first = (INPUT_START >> PAGE_SHIFT) as usize;
        let input_last = (INPUT_END >> PAGE_SHIFT) as usize;
        permissions[input_first..input_last].fill(PERM_READ);

        let stack_first = (STACK_START >> PAGE_SHIFT) as usize;
        let stack_last = (STACK_END >> PAGE_SHIFT) as usize;
        permissions[stack_first..stack_last].fill(PERM_READ | PERM_WRITE);

        Self {
            permissions: permissions.into(),
        }
    }

    fn clone_permissions(&self) -> Arc<[u8]> {
        Arc::clone(&self.permissions)
    }

    #[cfg(test)]
    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.permissions.as_ptr()
    }

    #[cfg(test)]
    pub(crate) fn strong_count(&self) -> usize {
        Arc::strong_count(&self.permissions)
    }

    #[cfg(test)]
    pub(crate) fn get(&self, page: usize) -> u8 {
        self.permissions[page]
    }
}

pub struct Memory {
    permissions: Arc<[u8]>,
    pages: Vec<Option<Page>>,
    address_space: Option<Box<[u8]>>,
}

/// A run-local, direct view of guest-memory metadata.
///
/// The pointers remain valid for the lifetime of this value. The exclusive
/// borrow carried by `_memory` prevents safe code from moving or mutating the
/// underlying [`Memory`] while a native runner is using them.
pub struct DirectMemory<'a> {
    permissions: *const u8,
    address_space: *mut u8,
    _memory: PhantomData<&'a mut Memory>,
}

impl DirectMemory<'_> {
    /// Returns the base of the immutable, page-indexed permission table.
    ///
    /// The table has [`RV32_PAGE_COUNT`] entries. Pages outside the project
    /// EEI address space are present with zero permissions, so native runners
    /// may validate any wrapping RV32 address without an out-of-bounds read.
    pub const fn permissions_ptr(&self) -> *const u8 {
        self.permissions
    }

    /// Returns the mutable base of the contiguous guest address space.
    ///
    /// The allocation has exactly [`ADDRESS_SPACE_SIZE`] bytes and remains
    /// stable for the lifetime of the owning [`Memory`]. Sparse pages are
    /// present as zero-filled bytes.
    pub const fn address_space_ptr(&self) -> *mut u8 {
        self.address_space
    }
}

impl Memory {
    pub fn new(image: &Image, input: &[u8]) -> Self {
        let permissions = PermissionTemplate::new(image);
        Self::from_permission_template(image, input, &permissions)
    }

    pub(crate) fn from_permission_template(
        image: &Image,
        input: &[u8],
        permissions: &PermissionTemplate,
    ) -> Self {
        let mut pages = image.pages.clone();
        let input_first = (INPUT_START >> PAGE_SHIFT) as usize;
        for (index, chunk) in input.chunks(PAGE_SIZE).enumerate() {
            if chunk.iter().any(|byte| *byte != 0) {
                let mut page = Box::new([0; PAGE_SIZE]);
                page[..chunk.len()].copy_from_slice(chunk);
                pages[input_first + index] = Some(page);
            }
        }

        Self {
            permissions: permissions.clone_permissions(),
            pages,
            address_space: None,
        }
    }

    pub(crate) fn from_permission_template_direct(
        image: &Image,
        input: &[u8],
        permissions: &PermissionTemplate,
    ) -> Self {
        let mut address_space = vec![0; ADDRESS_SPACE_SIZE as usize].into_boxed_slice();
        for (page_number, page) in image.pages.iter().enumerate() {
            let Some(page) = page else {
                continue;
            };
            let offset = page_number * PAGE_SIZE;
            address_space[offset..offset + PAGE_SIZE].copy_from_slice(page.as_ref());
        }

        for (index, chunk) in input.chunks(PAGE_SIZE).enumerate() {
            if chunk.iter().any(|byte| *byte != 0) {
                let offset = INPUT_START as usize + index * PAGE_SIZE;
                address_space[offset..offset + chunk.len()].copy_from_slice(chunk);
            }
        }

        Self {
            permissions: permissions.clone_permissions(),
            pages: Vec::new(),
            address_space: Some(address_space),
        }
    }

    fn ensure_address_space(&mut self) -> &mut Box<[u8]> {
        if self.address_space.is_none() {
            let mut address_space = vec![0; ADDRESS_SPACE_SIZE as usize].into_boxed_slice();
            for (page_number, page) in self.pages.iter_mut().enumerate() {
                let Some(page) = page.take() else {
                    continue;
                };
                let offset = page_number * PAGE_SIZE;
                address_space[offset..offset + PAGE_SIZE].copy_from_slice(page.as_ref());
            }
            self.pages = Vec::new();
            self.address_space = Some(address_space);
        }

        self.address_space
            .as_mut()
            .expect("direct address space was initialized above")
    }

    /// Creates a direct view for one run, initializing its bounded contiguous
    /// address space on first use.
    pub fn direct_memory(&mut self) -> DirectMemory<'_> {
        let permissions = self.permissions.as_ptr();
        let address_space = self.ensure_address_space().as_mut_ptr();
        DirectMemory {
            permissions,
            address_space,
            _memory: PhantomData,
        }
    }

    #[cfg(test)]
    pub(crate) fn permission_identity(&self) -> *const u8 {
        self.permissions.as_ptr()
    }

    #[cfg(test)]
    pub(crate) fn direct_memory_is_initialized(&self) -> bool {
        self.address_space.is_some() && self.pages.is_empty()
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
        if let Some(address_space) = &self.address_space {
            return address_space[address as usize];
        }
        self.pages[(address >> PAGE_SHIFT) as usize]
            .as_ref()
            .map_or(0, |page| page[address as usize & (PAGE_SIZE - 1)])
    }

    pub fn load_u16(&self, address: u32) -> u16 {
        if let Some(address_space) = &self.address_space {
            let offset = address as usize;
            return u16::from_le_bytes([address_space[offset], address_space[offset + 1]]);
        }
        let page = &self.pages[(address >> PAGE_SHIFT) as usize];
        let offset = address as usize & (PAGE_SIZE - 1);
        page.as_ref().map_or(0, |bytes| {
            u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
        })
    }

    pub fn load_u32(&self, address: u32) -> u32 {
        if let Some(address_space) = &self.address_space {
            let offset = address as usize;
            return u32::from_le_bytes([
                address_space[offset],
                address_space[offset + 1],
                address_space[offset + 2],
                address_space[offset + 3],
            ]);
        }
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
        let bytes = value.to_le_bytes();

        if let Some(address_space) = &mut self.address_space {
            let address = address as usize;
            address_space[address..address + size as usize]
                .copy_from_slice(&bytes[..size as usize]);
        } else {
            let page_number = (address >> PAGE_SHIFT) as usize;
            let offset = address as usize & (PAGE_SIZE - 1);
            let page = self.pages[page_number].get_or_insert_with(|| Box::new([0; PAGE_SIZE]));
            page[offset..offset + size as usize].copy_from_slice(&bytes[..size as usize]);
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
    use std::sync::Mutex;

    use super::*;

    static DIRECT_MEMORY_TEST_LOCK: Mutex<()> = Mutex::new(());

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
    fn direct_address_space_is_not_allocated_until_requested() {
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let memory = Memory::new(&image_with_resident_page(resident, PERM_READ), &[]);

        assert!(memory.address_space.is_none());
        assert_eq!(memory.pages.len(), PAGE_COUNT);
        assert!(memory.pages[resident].is_some());
    }

    #[test]
    fn direct_view_allocates_exact_address_space_and_converts_residency() {
        let _lock = DIRECT_MEMORY_TEST_LOCK.lock().unwrap();
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let sparse = resident + 1;
        let mut memory = Memory::new(
            &image_with_resident_page(resident, PERM_READ | PERM_EXEC),
            &[],
        );
        assert!(memory.address_space.is_none());

        let (permissions_ptr, address_space_ptr) = {
            let direct = memory.direct_memory();
            (direct.permissions_ptr(), direct.address_space_ptr())
        };

        let address_space = memory.address_space.as_ref().unwrap();
        assert_eq!(address_space.len(), ADDRESS_SPACE_SIZE as usize);
        assert_eq!(memory.permissions.len(), RV32_PAGE_COUNT);
        assert_eq!(permissions_ptr, memory.permissions.as_ptr());
        assert_eq!(address_space_ptr, address_space.as_ptr().cast_mut());
        assert!(memory.pages.is_empty());
        assert_eq!(memory.permissions[resident], PERM_READ | PERM_EXEC);
        assert_eq!(memory.permissions[sparse], 0);
        assert_eq!(memory.permissions[PAGE_COUNT], 0);
        assert_eq!(memory.permissions[RV32_PAGE_COUNT - 1], 0);
        assert_eq!(address_space[resident * PAGE_SIZE], 0xa5);
        assert!(
            address_space[sparse * PAGE_SIZE..(sparse + 1) * PAGE_SIZE]
                .iter()
                .all(|byte| *byte == 0)
        );
        assert_eq!(memory.load_u8(IMAGE_START), 0xa5);
    }

    #[test]
    fn stores_update_the_direct_address_space_after_every_write() {
        let _lock = DIRECT_MEMORY_TEST_LOCK.lock().unwrap();
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let address = STACK_START + 8;
        let mut memory = Memory::new(&image_with_resident_page(resident, PERM_READ), &[]);
        {
            let _direct = memory.direct_memory();
        }

        memory.store(address, 4, 0x4433_2211, IMAGE_START).unwrap();
        assert_eq!(memory.load_u32(address), 0x4433_2211);

        memory.store(address, 2, 0xbbaa, IMAGE_START).unwrap();
        assert_eq!(memory.load_u16(address), 0xbbaa);
    }

    #[test]
    fn failed_direct_backing_store_preserves_bytes() {
        let _lock = DIRECT_MEMORY_TEST_LOCK.lock().unwrap();
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let address = IMAGE_START + PAGE_SIZE as u32;
        let mut memory = Memory::new(&image_with_resident_page(resident, PERM_READ), &[]);
        {
            let _direct = memory.direct_memory();
        }

        assert_eq!(
            memory.store(address, 1, 0xaa, IMAGE_START),
            Err(GuestTrap::new("StoreAccessFault", IMAGE_START, address))
        );
        assert_eq!(memory.load_u8(address), 0);
    }

    #[test]
    fn direct_and_interpreter_access_share_the_authoritative_buffer() {
        let _lock = DIRECT_MEMORY_TEST_LOCK.lock().unwrap();
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let image = image_with_resident_page(resident, PERM_READ | PERM_WRITE);
        let mut memory = Memory::new(&image, &[]);
        let address = IMAGE_START + 16;

        {
            let _direct = memory.direct_memory();
        }
        memory.store(address, 4, 0x4433_2211, IMAGE_START).unwrap();
        let published_address_space = memory.direct_memory().address_space_ptr();
        assert_eq!(
            published_address_space,
            memory.address_space.as_ref().unwrap().as_ptr().cast_mut()
        );

        // The common crate forbids unsafe code, so model the native byte write
        // through the exact authoritative allocation exposed by the raw base.
        let address_space = memory.address_space.as_mut().unwrap();
        assert_eq!(address_space[address as usize], 0x11);
        address_space[address as usize + 1] = 0xaa;

        assert_eq!(memory.load_u32(address), 0x4433_aa11);
        assert_eq!(memory.read(address, 4), [0x11, 0xaa, 0x33, 0x44]);
        assert_eq!(
            memory.inspect(address, 4).unwrap(),
            [0x11, 0xaa, 0x33, 0x44]
        );
    }

    #[test]
    fn repeated_views_reuse_the_same_allocations() {
        let _lock = DIRECT_MEMORY_TEST_LOCK.lock().unwrap();
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let mut memory = Memory::new(&image_with_resident_page(resident, PERM_READ), &[]);

        let (first_permissions, first_address_space) = {
            let first = memory.direct_memory();
            (first.permissions_ptr(), first.address_space_ptr())
        };
        let second = memory.direct_memory();

        assert_eq!(second.permissions_ptr(), first_permissions);
        assert_eq!(second.address_space_ptr(), first_address_space);
    }

    #[test]
    fn direct_allocations_are_isolated_between_memory_instances() {
        let _lock = DIRECT_MEMORY_TEST_LOCK.lock().unwrap();
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let image = image_with_resident_page(resident, PERM_READ);
        let mut first_memory = Memory::new(&image, &[]);
        let mut second_memory = Memory::new(&image, &[]);

        let (first_permissions, first_address_space) = {
            let first = first_memory.direct_memory();
            (first.permissions_ptr(), first.address_space_ptr())
        };
        let second = second_memory.direct_memory();

        assert_ne!(second.permissions_ptr(), first_permissions);
        assert_ne!(second.address_space_ptr(), first_address_space);
    }
    #[test]
    fn oversized_image_cannot_populate_the_native_guard_tail() {
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let mut image = image_with_resident_page(resident, PERM_READ);
        image.permissions.push(PERM_READ | PERM_WRITE);

        let memory = Memory::new(&image, &[]);

        assert_eq!(PAGE_COUNT, 0x4000);
        assert_eq!(RV32_PAGE_COUNT, 0x10_0000);
        assert_eq!(memory.permissions.len(), RV32_PAGE_COUNT);
        assert_eq!(memory.permissions[resident], PERM_READ);
        assert!(
            memory.permissions[PAGE_COUNT..]
                .iter()
                .all(|&value| value == 0)
        );
    }

    #[test]
    fn permission_template_contains_fixed_eei_permissions_once() {
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let image = image_with_resident_page(resident, PERM_READ);
        let template = PermissionTemplate::new(&image);
        let input_first = (INPUT_START >> PAGE_SHIFT) as usize;
        let input_last = (INPUT_END >> PAGE_SHIFT) as usize - 1;
        let stack_first = (STACK_START >> PAGE_SHIFT) as usize;
        let stack_last = (STACK_END >> PAGE_SHIFT) as usize - 1;

        assert_eq!(template.get(resident), PERM_READ);
        assert_eq!(template.get(input_first), PERM_READ);
        assert_eq!(template.get(input_last), PERM_READ);
        assert_eq!(template.get(stack_first), PERM_READ | PERM_WRITE);
        assert_eq!(template.get(stack_last), PERM_READ | PERM_WRITE);
        assert_eq!(template.get(PAGE_COUNT), 0);
        assert_eq!(template.get(RV32_PAGE_COUNT - 1), 0);
    }

    #[test]
    fn permission_template_is_shared_but_run_data_is_not() {
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let image = image_with_resident_page(resident, PERM_READ);
        let template = PermissionTemplate::new(&image);
        let mut first = Memory::from_permission_template(&image, &[], &template);
        let mut second = Memory::from_permission_template(&image, &[], &template);

        assert_eq!(first.permission_identity(), template.as_ptr());
        assert_eq!(second.permission_identity(), template.as_ptr());
        assert_eq!(template.strong_count(), 3);

        let first_address_space = first.direct_memory().address_space_ptr();
        let second_address_space = second.direct_memory().address_space_ptr();
        assert_ne!(first_address_space, second_address_space);
    }

    #[test]
    fn direct_template_construction_skips_sparse_page_cloning() {
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let image = image_with_resident_page(resident, PERM_READ);
        let template = PermissionTemplate::new(&image);
        let input = [0x11, 0x22, 0x33, 0x44];
        let memory = Memory::from_permission_template_direct(&image, &input, &template);

        assert!(memory.pages.is_empty());
        let address_space = memory.address_space.as_ref().unwrap();
        assert_eq!(address_space[resident * PAGE_SIZE], 0xa5);
        assert_eq!(
            &address_space[INPUT_START as usize..INPUT_START as usize + input.len()],
            &input
        );
        assert_eq!(memory.permission_identity(), template.as_ptr());
    }

    #[test]
    fn input_and_stores_remain_fresh_between_runs() {
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let image = image_with_resident_page(resident, PERM_READ);
        let input = [0x11, 0x22, 0x33, 0x44];
        let mut first = Memory::new(&image, &input);
        let second = Memory::new(&image, &input);

        assert_eq!(first.load_u32(INPUT_START), 0x4433_2211);
        first.store(STACK_START, 4, 0x8877_6655, 0).unwrap();

        assert_eq!(first.load_u32(STACK_START), 0x8877_6655);
        assert_eq!(second.load_u32(STACK_START), 0);
    }

    #[test]
    fn empty_read_accepts_any_address_and_end_inspection() {
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let image = image_with_resident_page(resident, PERM_READ);
        let memory = Memory::new(&image, &[]);

        assert!(memory.read(u32::MAX, 0).is_empty());
        assert_eq!(memory.inspect(ADDRESS_SPACE_SIZE, 0), Ok(Vec::new()));
    }

    #[test]
    fn repeated_direct_views_publish_the_same_allocations() {
        let resident = (IMAGE_START >> PAGE_SHIFT) as usize;
        let mut memory = Memory::new(&image_with_resident_page(resident, PERM_READ), &[]);

        let (first_permissions, first_data) = {
            let direct = memory.direct_memory();
            (direct.permissions_ptr(), direct.address_space_ptr())
        };
        let direct = memory.direct_memory();

        assert_eq!(direct.permissions_ptr(), first_permissions);
        assert_eq!(direct.address_space_ptr(), first_data);
    }
}
