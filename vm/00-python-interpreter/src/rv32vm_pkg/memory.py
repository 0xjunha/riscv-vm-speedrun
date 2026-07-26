"""Sparse, page-permission-aware memory for the 64 MiB guest space."""

from __future__ import annotations

from .constants import (
    ADDRESS_SPACE_SIZE,
    INPUT_END,
    INPUT_START,
    PAGE_COUNT,
    PAGE_MASK,
    PAGE_SHIFT,
    PAGE_SIZE,
    PERM_READ,
    PERM_WRITE,
    STACK_END,
    STACK_START,
)
from .errors import GuestTrap

_ZERO_PAGE = bytes(PAGE_SIZE)


class Memory:
    """A fresh run's memory, with immutable clean pages and copy-on-write stores."""

    __slots__ = ("pages", "permissions")

    def __init__(
        self,
        image_permissions: bytes,
        image_pages: dict[int, bytes],
        input_data: bytes,
    ) -> None:
        self.permissions = bytearray(image_permissions)
        # Bytes values are pristine pages; a first store replaces one with bytearray.
        self.pages: dict[int, bytes | bytearray] = dict(image_pages)

        input_first = INPUT_START >> PAGE_SHIFT
        input_last = INPUT_END >> PAGE_SHIFT
        self.permissions[input_first:input_last] = bytes([PERM_READ]) * (
            input_last - input_first
        )
        for offset in range(0, len(input_data), PAGE_SIZE):
            chunk = input_data[offset : offset + PAGE_SIZE]
            if any(chunk):
                if len(chunk) != PAGE_SIZE:
                    chunk = chunk + bytes(PAGE_SIZE - len(chunk))
                self.pages[input_first + offset // PAGE_SIZE] = bytes(chunk)

        stack_first = STACK_START >> PAGE_SHIFT
        stack_last = STACK_END >> PAGE_SHIFT
        self.permissions[stack_first:stack_last] = bytes([PERM_READ | PERM_WRITE]) * (
            stack_last - stack_first
        )

    @staticmethod
    def _host_range_valid(address: int, size: int) -> bool:
        return (
            isinstance(address, int)
            and isinstance(size, int)
            and address >= 0
            and size >= 0
            and address <= ADDRESS_SPACE_SIZE
            and address + size <= ADDRESS_SPACE_SIZE
        )

    def check(
        self, address: int, size: int, permission: int, cause: str, pc: int
    ) -> None:
        """Validate the complete non-wrapping byte range before any mutation."""
        if not self._host_range_valid(address, size):
            raise GuestTrap(cause, pc, address & 0xFFFF_FFFF)
        if size == 0:
            return
        first = address >> PAGE_SHIFT
        last = (address + size - 1) >> PAGE_SHIFT
        permissions = self.permissions
        for page in range(first, last + 1):
            if permissions[page] & permission != permission:
                raise GuestTrap(cause, pc, address & 0xFFFF_FFFF)

    def read_checked(
        self, address: int, size: int, permission: int, cause: str, pc: int
    ) -> bytes:
        self.check(address, size, permission, cause, pc)
        return self.read_unchecked(address, size)

    def read_unchecked(self, address: int, size: int) -> bytes:
        if size == 0:
            return b""
        page_number = address >> PAGE_SHIFT
        page_offset = address & PAGE_MASK
        if page_offset + size <= PAGE_SIZE:
            page = self.pages.get(page_number)
            if page is None:
                return _ZERO_PAGE[page_offset : page_offset + size]
            return bytes(page[page_offset : page_offset + size])

        result = bytearray()
        remaining = size
        cursor = address
        while remaining:
            page_number = cursor >> PAGE_SHIFT
            page_offset = cursor & PAGE_MASK
            take = min(remaining, PAGE_SIZE - page_offset)
            page = self.pages.get(page_number)
            if page is None:
                result.extend(_ZERO_PAGE[page_offset : page_offset + take])
            else:
                result.extend(page[page_offset : page_offset + take])
            cursor += take
            remaining -= take
        return bytes(result)

    def load_u8(self, address: int) -> int:
        page = self.pages.get(address >> PAGE_SHIFT)
        if page is None:
            return 0
        return page[address & PAGE_MASK]

    def load_u16(self, address: int) -> int:
        page = self.pages.get(address >> PAGE_SHIFT)
        offset = address & PAGE_MASK
        if page is None:
            return 0
        return page[offset] | (page[offset + 1] << 8)

    def load_u32(self, address: int) -> int:
        page = self.pages.get(address >> PAGE_SHIFT)
        offset = address & PAGE_MASK
        if page is None:
            return 0
        return (
            page[offset]
            | (page[offset + 1] << 8)
            | (page[offset + 2] << 16)
            | (page[offset + 3] << 24)
        )

    def store_checked(
        self, address: int, size: int, value: int, cause: str, pc: int
    ) -> None:
        self.check(address, size, PERM_WRITE, cause, pc)
        # Aligned EEI halfword/word stores cannot cross a 4 KiB page.
        page_number = address >> PAGE_SHIFT
        page = self.pages.get(page_number)
        if not isinstance(page, bytearray):
            page = bytearray(_ZERO_PAGE if page is None else page)
            self.pages[page_number] = page
        offset = address & PAGE_MASK
        for index in range(size):
            page[offset + index] = (value >> (index * 8)) & 0xFF

    def inspect(self, address: int, size: int) -> bytes:
        """Read mapped memory without applying guest read permissions."""
        if not self._host_range_valid(address, size):
            raise ValueError("inspect range is outside guest address space")
        if size:
            first = address >> PAGE_SHIFT
            last = (address + size - 1) >> PAGE_SHIFT
            for page in range(first, last + 1):
                if not self.permissions[page]:
                    raise ValueError("inspect range includes unmapped memory")
        return self.read_unchecked(address, size)


def empty_permissions() -> bytearray:
    return bytearray(PAGE_COUNT)
