"""Build and validate the small ELF32 images used by contract tests."""

from __future__ import annotations

import struct
from dataclasses import dataclass, field

ELF_HEADER = struct.Struct("<16sHHIIIIIHHHHHH")
PROGRAM_HEADER = struct.Struct("<IIIIIIII")
SECTION_HEADER = struct.Struct("<IIIIIIIIII")

ET_EXEC = 2
EM_RISCV = 243
EV_CURRENT = 1

PT_LOAD = 1
PT_DYNAMIC = 2
PT_INTERP = 3
PT_TLS = 7
PT_GNU_STACK = 0x6474E551

PF_X = 1
PF_W = 2
PF_R = 4

SHT_RELA = 4
SHT_DYNAMIC = 6
SHT_REL = 9
SHF_ALLOC = 0x2
SHF_TLS = 0x400

IMAGE_START = 0x0001_0000
IMAGE_END = 0x0300_0000
CODE_ADDRESS = IMAGE_START
DATA_ADDRESS = 0x0002_0000
PAGE_SIZE = 0x1000


@dataclass
class Segment:
    data: bytes = b""
    address: int = DATA_ADDRESS
    memory_size: int | None = None
    flags: int = PF_R | PF_W
    alignment: int = PAGE_SIZE
    kind: int = PT_LOAD
    offset: int | None = None
    file_size: int | None = None
    store_data: bool = True


@dataclass
class Section:
    kind: int = 0
    flags: int = 0
    size: int = 0
    info: int = 0


@dataclass
class ElfSpec:
    entry: int
    segments: list[Segment]
    sections: list[Section] = field(default_factory=list)
    header: dict[str, int] = field(default_factory=dict)


def _align(value: int, alignment: int) -> int:
    return (value + alignment - 1) & -alignment


def _base_spec(code: bytes) -> ElfSpec:
    return ElfSpec(
        CODE_ADDRESS,
        [
            Segment(
                code,
                CODE_ADDRESS,
                len(code),
                PF_R | PF_X,
                PAGE_SIZE,
                offset=PAGE_SIZE,
            ),
            Segment(
                b"\x29\x00\x00\x00",
                DATA_ADDRESS,
                PAGE_SIZE,
                PF_R | PF_W,
                PAGE_SIZE,
                offset=2 * PAGE_SIZE,
            ),
        ],
    )


def _encode(spec: ElfSpec) -> bytes:
    phoff = spec.header.get("phoff", ELF_HEADER.size)
    offsets: list[int] = []
    next_offset = PAGE_SIZE
    for segment in spec.segments:
        offset = segment.offset
        if offset is None:
            offset = next_offset
        offsets.append(offset)
        if segment.store_data:
            next_offset = _align(
                max(next_offset, offset + len(segment.data)), PAGE_SIZE
            )

    file_end = max(ELF_HEADER.size, phoff + len(spec.segments) * PROGRAM_HEADER.size)
    for segment, offset in zip(spec.segments, offsets):
        if segment.store_data:
            file_end = max(file_end, offset + len(segment.data))

    actual_shoff = 0
    if spec.sections:
        actual_shoff = _align(file_end, 4)
        file_end = actual_shoff + len(spec.sections) * SECTION_HEADER.size
    header_shoff = spec.header.get("shoff", actual_shoff)

    data = bytearray(file_end)
    ident = bytearray(16)
    ident[:7] = b"\x7fELF\x01\x01\x01"
    values = {
        "type": ET_EXEC,
        "machine": EM_RISCV,
        "version": EV_CURRENT,
        "entry": spec.entry,
        "phoff": phoff,
        "shoff": header_shoff,
        "flags": 0,
        "ehsize": ELF_HEADER.size,
        "phentsize": PROGRAM_HEADER.size,
        "phnum": len(spec.segments),
        "shentsize": SECTION_HEADER.size,
        "shnum": len(spec.sections),
        "shstrndx": 0,
    }
    values.update(spec.header)
    ELF_HEADER.pack_into(
        data,
        0,
        bytes(ident),
        values["type"],
        values["machine"],
        values["version"],
        values["entry"],
        values["phoff"],
        values["shoff"],
        values["flags"],
        values["ehsize"],
        values["phentsize"],
        values["phnum"],
        values["shentsize"],
        values["shnum"],
        values["shstrndx"],
    )

    for index, (segment, offset) in enumerate(zip(spec.segments, offsets)):
        file_size = (
            len(segment.data) if segment.file_size is None else segment.file_size
        )
        memory_size = file_size if segment.memory_size is None else segment.memory_size
        PROGRAM_HEADER.pack_into(
            data,
            phoff + index * PROGRAM_HEADER.size,
            segment.kind,
            offset,
            segment.address,
            segment.address,
            file_size,
            memory_size,
            segment.flags,
            segment.alignment,
        )
        if segment.store_data:
            data[offset : offset + len(segment.data)] = segment.data

    if spec.sections:
        for index, section in enumerate(spec.sections):
            SECTION_HEADER.pack_into(
                data,
                actual_shoff + index * SECTION_HEADER.size,
                0,
                section.kind,
                section.flags,
                0,
                0,
                section.size,
                0,
                section.info,
                0,
                0,
            )
    return bytes(data)


def _patch(data: bytes, offset: int, layout: str, value: int | bytes) -> bytes:
    changed = bytearray(data)
    if isinstance(value, bytes):
        changed[offset : offset + len(value)] = value
    else:
        struct.pack_into(layout, changed, offset, value)
    return bytes(changed)


def build_elf(code: bytes, variant: str = "default") -> bytes:
    """Build one valid or deliberately invalid contract ELF variant."""

    spec = _base_spec(code)

    if variant == "default":
        return _encode(spec)
    if variant in {"align-0", "align-1"}:
        alignment = int(variant[-1])
        for segment in spec.segments:
            segment.alignment = alignment
        return _encode(spec)
    if variant == "section-table":
        spec.sections = [Section()]
        return _encode(spec)
    if variant == "read-only-segment":
        spec.segments.append(
            Segment(b"RO\0\0", 0x0003_0000, 4, PF_R, PAGE_SIZE, offset=3 * PAGE_SIZE)
        )
        return _encode(spec)
    if variant == "compatible-same-page":
        address = CODE_ADDRESS + _align(len(code), 4)
        spec.segments.append(
            Segment(b"RO\0\0", address, 4, PF_R, 1, offset=3 * PAGE_SIZE)
        )
        return _encode(spec)
    if variant in {"nonallocated-dynamic", "nonallocated-rel", "nonallocated-rela"}:
        section_kind = {
            "nonallocated-dynamic": SHT_DYNAMIC,
            "nonallocated-rel": SHT_REL,
            "nonallocated-rela": SHT_RELA,
        }[variant]
        spec.sections = [Section(), Section(section_kind, 0)]
        return _encode(spec)
    if variant == "nonexec-gnu-stack":
        spec.segments.append(Segment(kind=PT_GNU_STACK, flags=PF_R | PF_W, alignment=1))
        return _encode(spec)
    if variant == "empty-load-ignored-fields":
        spec.segments.append(
            Segment(
                address=0xFFFF_FFFF,
                memory_size=0,
                flags=0xFFFF_FFFF,
                alignment=1,
                offset=0,
                file_size=0,
            )
        )
        return _encode(spec)

    if variant == "bad-magic":
        return _patch(_encode(spec), 0, "", b"NOPE")
    if variant == "not-elf32":
        return _patch(_encode(spec), 4, "<B", 2)
    if variant == "not-little-endian":
        return _patch(_encode(spec), 5, "<B", 2)
    if variant == "bad-ident-version":
        return _patch(_encode(spec), 6, "<B", 2)
    if variant == "bad-header-version":
        spec.header["version"] = 2
    elif variant == "not-et-exec":
        spec.header["type"] = 3
    elif variant == "not-riscv":
        spec.header["machine"] = 62
    elif variant == "unsupported-flags":
        spec.header["flags"] = 1
    elif variant == "bad-elf-header-size":
        spec.header["ehsize"] = 0
    elif variant == "bad-program-header-size":
        spec.header["phentsize"] = 0
    elif variant == "zero-program-headers":
        spec.header["phnum"] = 0
    elif variant == "extended-program-headers":
        spec.sections = [Section(info=len(spec.segments))]
        spec.header["phnum"] = 0xFFFF
    elif variant == "misaligned-entry":
        spec.entry += 2
    elif variant == "truncated-header":
        return _encode(spec)[: ELF_HEADER.size - 1]
    elif variant == "truncated-program-headers":
        return _encode(spec)[: ELF_HEADER.size + 8]
    elif variant == "no-load-segment":
        for segment in spec.segments:
            segment.kind = 4
    elif variant == "filesz-exceeds-memsz":
        spec.segments[1].data = b"12345"
        spec.segments[1].file_size = 5
        spec.segments[1].memory_size = 4
    elif variant == "segment-outside-image":
        spec.segments[1].address = IMAGE_END - PAGE_SIZE
        spec.segments[1].memory_size = 2 * PAGE_SIZE
    elif variant == "overlapping-segments":
        spec.segments[1].address = CODE_ADDRESS
        spec.segments[1].alignment = 1
        spec.segments[1].flags = PF_R
    elif variant == "writable-executable":
        spec.segments[1].flags = PF_R | PF_W | PF_X
    elif variant == "entry-not-executable":
        spec.entry = DATA_ADDRESS
    elif variant == "bad-offset-address-alignment":
        spec.segments[1].address += 4
    elif variant == "load-file-range-outside":
        spec.segments[1].offset = 0x0090_0000
        spec.segments[1].store_data = False
    elif variant == "non-power-of-two-alignment":
        spec.segments[1].alignment = 3
    elif variant == "execute-only-segment":
        spec.segments[1].flags = PF_X
    elif variant == "write-only-segment":
        spec.segments[1].flags = PF_W
    elif variant == "unknown-segment-permission":
        spec.segments[1].flags = 8
    elif variant == "page-permission-union-wx":
        address = CODE_ADDRESS + _align(len(code), 4)
        spec.segments.append(
            Segment(b"RW\0\0", address, 4, PF_R | PF_W, 1, offset=3 * PAGE_SIZE)
        )
    elif variant in {
        "rx-vaddr-not-four-aligned",
        "rx-filesz-not-four-granular",
        "rx-memsz-not-four-granular",
    }:
        segment = Segment(
            b"\0\0\0\0",
            0x0003_0000,
            4,
            PF_R | PF_X,
            1,
            offset=3 * PAGE_SIZE,
        )
        if variant == "rx-vaddr-not-four-aligned":
            segment.address += 2
        elif variant == "rx-filesz-not-four-granular":
            segment.data = b"\0" * 5
            segment.file_size = 5
            segment.memory_size = 8
        else:
            segment.memory_size = 5
        spec.segments.append(segment)
    elif variant in {"dynamic-segment", "interpreter-segment", "tls-segment"}:
        kind = {
            "dynamic-segment": PT_DYNAMIC,
            "interpreter-segment": PT_INTERP,
            "tls-segment": PT_TLS,
        }[variant]
        spec.segments.append(Segment(kind=kind, alignment=1))
    elif variant == "executable-gnu-stack":
        spec.segments.append(Segment(kind=PT_GNU_STACK, flags=PF_X, alignment=1))
    elif variant == "only-empty-load-segments":
        for segment in spec.segments:
            segment.data = b""
            segment.file_size = 0
            segment.memory_size = 0
    elif variant == "empty-load-invalid-file-range":
        spec.segments.append(
            Segment(
                memory_size=0,
                offset=0x0090_0000,
                file_size=0,
                store_data=False,
            )
        )
    elif variant == "empty-load-invalid-alignment":
        spec.segments.append(Segment(memory_size=0, alignment=3))
    elif variant in {
        "allocated-dynamic-section",
        "allocated-rel-section",
        "allocated-rela-section",
        "tls-section",
        "truncated-section-table",
        "zero-section-offset",
        "bad-section-name-index",
        "bad-section-header-size",
    }:
        section_kind = {
            "allocated-dynamic-section": SHT_DYNAMIC,
            "allocated-rel-section": SHT_REL,
            "allocated-rela-section": SHT_RELA,
            "tls-section": 1,
        }.get(variant, 0)
        section_flags = SHF_TLS if variant == "tls-section" else SHF_ALLOC
        spec.sections = [Section(), Section(section_kind, section_flags)]
        if variant == "zero-section-offset":
            spec.header["shoff"] = 0
        elif variant == "bad-section-name-index":
            spec.header["shstrndx"] = 2
        elif variant == "bad-section-header-size":
            spec.header["shentsize"] = SECTION_HEADER.size - 1
        encoded = _encode(spec)
        if variant == "truncated-section-table":
            return encoded[:-1]
        return encoded
    elif variant == "section-name-index-without-sections":
        spec.header["shstrndx"] = 1
    elif variant == "extended-section-numbering":
        spec.sections = [Section(size=1)]
        spec.header["shnum"] = 0
    else:
        raise ValueError(f"unknown ELF variant: {variant}")
    return _encode(spec)


def validate_elf(data: bytes) -> str | None:
    """Return the first violated ELF contract rule, or ``None`` when valid."""

    if len(data) < ELF_HEADER.size:
        return "truncated-header"
    (
        ident,
        elf_type,
        machine,
        version,
        entry,
        phoff,
        shoff,
        flags,
        ehsize,
        phentsize,
        phnum,
        shentsize,
        shnum,
        shstrndx,
    ) = ELF_HEADER.unpack_from(data)

    if ident[:4] != b"\x7fELF":
        return "bad-magic"
    if ident[4] != 1:
        return "not-elf32"
    if ident[5] != 1:
        return "not-little-endian"
    if ident[6] != EV_CURRENT:
        return "bad-ident-version"
    if version != EV_CURRENT:
        return "bad-header-version"
    if elf_type != ET_EXEC:
        return "not-et-exec"
    if machine != EM_RISCV:
        return "not-riscv"
    if flags != 0:
        return "unsupported-flags"
    if ehsize != ELF_HEADER.size:
        return "bad-elf-header-size"
    if phnum == 0:
        return "zero-program-headers"
    if phnum == 0xFFFF:
        return "extended-program-headers"
    if phentsize != PROGRAM_HEADER.size:
        return "bad-program-header-size"
    if phoff > len(data) or phoff + phnum * phentsize > len(data):
        return "truncated-program-headers"

    if shnum:
        if shoff == 0:
            return "zero-section-offset"
        if shentsize != SECTION_HEADER.size:
            return "bad-section-header-size"
        if shoff > len(data) or shoff + shnum * shentsize > len(data):
            return "truncated-section-table"
        if shstrndx >= shnum:
            return "bad-section-name-index"
        for index in range(shnum):
            section = SECTION_HEADER.unpack_from(data, shoff + index * shentsize)
            section_kind, section_flags = section[1], section[2]
            if section_kind == SHT_DYNAMIC and section_flags & SHF_ALLOC:
                return "allocated-dynamic-section"
            if section_kind == SHT_REL and section_flags & SHF_ALLOC:
                return "allocated-rel-section"
            if section_kind == SHT_RELA and section_flags & SHF_ALLOC:
                return "allocated-rela-section"
            if section_flags & SHF_TLS:
                return "tls-section"
    elif shoff != 0:
        return "extended-section-numbering"
    elif shstrndx != 0:
        return "section-name-index-without-sections"

    ranges: list[tuple[int, int]] = []
    page_permissions: dict[int, int] = {}
    saw_load = False
    loads = 0
    for index in range(phnum):
        (
            kind,
            offset,
            address,
            _physical,
            file_size,
            memory_size,
            segment_flags,
            alignment,
        ) = PROGRAM_HEADER.unpack_from(data, phoff + index * phentsize)
        if kind == PT_DYNAMIC:
            return "dynamic-segment"
        if kind == PT_INTERP:
            return "interpreter-segment"
        if kind == PT_TLS:
            return "tls-segment"
        if kind == PT_GNU_STACK and segment_flags & PF_X:
            return "executable-gnu-stack"
        if kind != PT_LOAD:
            continue
        saw_load = True
        if file_size > memory_size:
            return "filesz-exceeds-memsz"
        if alignment not in (0, 1):
            if alignment & (alignment - 1):
                return "non-power-of-two-alignment"
            if (address - offset) & (alignment - 1):
                return "bad-offset-address-alignment"
        if offset > len(data) or offset + file_size > len(data):
            return "load-file-range-outside"
        if memory_size == 0:
            continue

        loads += 1
        end = address + memory_size
        if address < IMAGE_START or end > IMAGE_END:
            return "segment-outside-image"
        for other_start, other_end in ranges:
            if address < other_end and other_start < end:
                return "overlapping-segments"
        ranges.append((address, end))

        if segment_flags == PF_R:
            permission = PF_R
        elif segment_flags == PF_R | PF_W:
            permission = PF_R | PF_W
        elif segment_flags == PF_R | PF_X:
            permission = PF_R | PF_X
        elif segment_flags & PF_W and segment_flags & PF_X:
            return "writable-executable"
        elif segment_flags == PF_X:
            return "execute-only-segment"
        elif segment_flags == PF_W:
            return "write-only-segment"
        else:
            return "unknown-segment-permission"

        if permission & PF_X:
            if address & 3:
                return "rx-vaddr-not-four-aligned"
            if file_size & 3:
                return "rx-filesz-not-four-granular"
            if memory_size & 3:
                return "rx-memsz-not-four-granular"
        first_page = address // PAGE_SIZE
        last_page = (end - 1) // PAGE_SIZE
        for page in range(first_page, last_page + 1):
            combined = page_permissions.get(page, 0) | permission
            if combined & PF_W and combined & PF_X:
                return "page-permission-union-wx"
            page_permissions[page] = combined

    if not saw_load:
        return "no-load-segment"
    if loads == 0:
        return "only-empty-load-segments"
    if entry & 3:
        return "misaligned-entry"
    if entry >= IMAGE_END or not page_permissions.get(entry // PAGE_SIZE, 0) & PF_X:
        return "entry-not-executable"
    return None
