from __future__ import annotations

import gc
import io
import struct
import weakref

import pytest
from rv32vm_pkg.elf import ELF_HEADER, PROGRAM_HEADER, load_elf
from rv32vm_pkg.machine import LoadedProgram, Machine
from rv32vm_pkg.protocol import (
    HEADER,
    MAGIC,
    OP_LOAD,
    OP_RESET,
    OP_RUN,
    OP_SHUTDOWN,
    OP_UNLOAD,
    RUN_HEADER,
    VERSION,
    serve,
)

CODE_ADDRESS = 0x0001_0000


def _i_type(
    immediate: int,
    rs1: int,
    funct3: int,
    rd: int,
    opcode: int = 0x13,
) -> int:
    return (
        ((immediate & 0xFFF) << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | opcode
    )


def _elf(*instructions: int, code_address: int = CODE_ADDRESS) -> bytes:
    code = struct.pack(f"<{len(instructions)}I", *instructions)
    code_offset = 0x100
    ident = b"\x7fELF\x01\x01\x01" + bytes(9)
    header = ELF_HEADER.pack(
        ident,
        2,
        243,
        1,
        code_address,
        ELF_HEADER.size,
        0,
        0,
        ELF_HEADER.size,
        PROGRAM_HEADER.size,
        1,
        0,
        0,
        0,
    )
    program_header = PROGRAM_HEADER.pack(
        1,
        code_offset,
        code_address,
        code_address,
        len(code),
        len(code),
        5,
        4,
    )
    return (
        header
        + program_header
        + bytes(code_offset - len(header + program_header))
        + code
    )


def test_cache_is_reused_without_sharing_input_or_registers(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # LW a0, 0(a0); ECALL exit.
    program = LoadedProgram(_elf(_i_type(0, 10, 2, 10, 0x03), 0x0000_0073))
    first = program.new_machine(struct.pack("<I", 11), 0)
    assert first.run(10)["exit_code"] == 11
    cached_block = program.block_cache[CODE_ADDRESS]
    monkeypatch.setattr(
        "rv32vm_pkg.machine._translate_block",
        lambda *_: pytest.fail("cached block was decoded again"),
    )

    second = program.new_machine(struct.pack("<I", 29), 0)
    assert second.registers[10] != first.registers[10]
    assert second.run(10)["exit_code"] == 29
    assert program.block_cache[CODE_ADDRESS] is cached_block


def test_instruction_limit_is_checked_inside_and_between_blocks() -> None:
    add = _i_type(1, 1, 0, 1)
    program = LoadedProgram(_elf(*(add for _ in range(70)), 0x0000_0073))

    first = program.new_machine(b"", 0)
    result = first.run(64)
    assert result["status"] == "resource_failure"
    assert result["retired_instructions"] == 64
    assert first.pc == CODE_ADDRESS + 64 * 4
    assert first.registers[1] == 64
    assert tuple(program.block_cache) == (CODE_ADDRESS,)

    second = program.new_machine(b"", 0)
    result = second.run(65)
    assert result["status"] == "resource_failure"
    assert result["retired_instructions"] == 65
    assert second.pc == CODE_ADDRESS + 65 * 4
    assert second.registers[1] == 65
    assert CODE_ADDRESS + 64 * 4 in program.block_cache


def test_fetch_fault_after_valid_prefix_is_deferred_until_prefix_commits() -> None:
    code_address = CODE_ADDRESS + 0xFF8
    image = load_elf(
        _elf(
            _i_type(1, 0, 0, 1),
            _i_type(2, 0, 0, 2),
            code_address=code_address,
        )
    )
    machine = Machine(image, b"", 0)

    result = machine.run(10)

    assert result["status"] == "trap"
    assert result["trap"] == {
        "cause": "InstructionAccessFault",
        "pc": code_address + 8,
        "value": code_address + 8,
    }
    assert result["retired_instructions"] == 2
    assert machine.registers[1:3] == [1, 2]


def test_illegal_instruction_in_cached_block_traps_precisely() -> None:
    illegal_slli = _i_type(0x20, 0, 1, 2)
    image = load_elf(
        _elf(
            _i_type(7, 0, 0, 1),
            illegal_slli,
            _i_type(9, 0, 0, 3),
        )
    )
    machine = Machine(image, b"", 0)

    result = machine.run(10)

    assert result["trap"] == {
        "cause": "IllegalInstruction",
        "pc": CODE_ADDRESS + 4,
        "value": illegal_slli,
    }
    assert result["retired_instructions"] == 1
    assert machine.registers[1] == 7
    assert machine.registers[2] == 0x0400_0000
    assert machine.registers[3] == 0


def test_protocol_reuses_cache_across_reset_and_releases_it_at_unload(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    cache_reference = None
    cache_ids = []
    observations = []

    class Cache:
        pass

    class ObservedMachine:
        def __init__(self, cache: Cache) -> None:
            self.cache = cache
            self.output = b""

        def run(self, _instruction_limit: int) -> dict[str, object]:
            return {
                "schema_version": 1,
                "status": "exit",
                "exit_code": 0,
                "trap": None,
                "resource_failure": None,
                "retired_instructions": 1,
                "output_length": 0,
            }

    class ObservedProgram:
        def __init__(self, _payload: bytes) -> None:
            nonlocal cache_reference
            self.cache = Cache()
            cache_reference = weakref.ref(self.cache)

        def new_machine(
            self, _input_data: bytes, _output_limit: int
        ) -> ObservedMachine:
            cache_ids.append(id(self.cache))
            return ObservedMachine(self.cache)

    class ObservingOutput(io.BytesIO):
        flush_count = 0

        def flush(self) -> None:
            super().flush()
            self.flush_count += 1
            if self.flush_count in (3, 4, 5, 6):
                gc.collect()
                assert cache_reference is not None
                observations.append((self.flush_count, cache_reference() is not None))

    def request(opcode: int, request_id: int, payload: bytes = b"") -> bytes:
        return (
            HEADER.pack(MAGIC, VERSION, opcode, 0, request_id, len(payload)) + payload
        )

    monkeypatch.setattr("rv32vm_pkg.protocol.LoadedProgram", ObservedProgram)
    run_payload = RUN_HEADER.pack(1, 0, 0)
    requests = b"".join(
        (
            request(OP_LOAD, 1, b"ELF"),
            request(OP_RUN, 2, run_payload),
            request(OP_RESET, 3),
            request(OP_RUN, 4, run_payload),
            request(OP_UNLOAD, 5),
            request(OP_SHUTDOWN, 6),
        )
    )

    assert serve(io.BytesIO(requests), ObservingOutput()) == 0
    assert cache_ids[0] == cache_ids[1]
    assert observations == [(3, True), (4, True), (5, True), (6, False)]
