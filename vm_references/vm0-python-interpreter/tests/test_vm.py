from __future__ import annotations

import io
import json
import struct
import tempfile
import unittest
from pathlib import Path

from rv32vm_pkg.cli import main
from rv32vm_pkg.elf import (
    ELF_HEADER,
    PROGRAM_HEADER,
    SECTION_HEADER,
    ElfError,
    load_elf,
)
from rv32vm_pkg.machine import Machine
from rv32vm_pkg.protocol import (
    HEADER,
    MAGIC,
    OP_LOAD,
    OP_RESET,
    OP_RUN,
    OP_SHUTDOWN,
    OP_UNLOAD,
    RUN_HEADER,
    RUN_RESPONSE_HEADER,
    STATUS_INVALID_PAYLOAD,
    STATUS_MALFORMED_FRAME,
    STATUS_OK,
    VERSION,
    serve,
)

CODE_ADDRESS = 0x0001_0000
DATA_ADDRESS = 0x0002_0000


def i_type(immediate: int, rs1: int, funct3: int, rd: int, opcode: int = 0x13) -> int:
    return (
        ((immediate & 0xFFF) << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | opcode
    )


def r_type(funct7: int, rs2: int, rs1: int, funct3: int, rd: int) -> int:
    return (
        (funct7 << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x33
    )


def u_type(upper: int, rd: int, opcode: int = 0x37) -> int:
    return ((upper & 0xFFFFF) << 12) | (rd << 7) | opcode


def s_type(immediate: int, rs2: int, rs1: int, funct3: int) -> int:
    immediate &= 0xFFF
    return (
        ((immediate >> 5) << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (funct3 << 12)
        | ((immediate & 0x1F) << 7)
        | 0x23
    )


def b_type(immediate: int, rs2: int, rs1: int, funct3: int) -> int:
    immediate &= 0x1FFF
    return (
        (((immediate >> 12) & 1) << 31)
        | (((immediate >> 5) & 0x3F) << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (funct3 << 12)
        | (((immediate >> 1) & 0xF) << 8)
        | (((immediate >> 11) & 1) << 7)
        | 0x63
    )


def j_type(immediate: int, rd: int) -> int:
    immediate &= 0x1FFFFF
    return (
        (((immediate >> 20) & 1) << 31)
        | (((immediate >> 1) & 0x3FF) << 21)
        | (((immediate >> 11) & 1) << 20)
        | (((immediate >> 12) & 0xFF) << 12)
        | (rd << 7)
        | 0x6F
    )


def words(*instructions: int) -> bytes:
    return struct.pack(f"<{len(instructions)}I", *instructions)


def make_elf(
    code: bytes,
    *,
    data: bytes = b"",
    data_memory_size: int | None = None,
    code_flags: int = 5,
    data_flags: int = 6,
    entry: int = CODE_ADDRESS,
    overlap: bool = False,
    elf_flags: int = 0,
) -> bytes:
    segments = 1 + int(bool(data) or data_memory_size is not None)
    code_offset = 0x100
    data_offset = (code_offset + len(code) + 0xFF) & ~0xFF
    ident = b"\x7fELF\x01\x01\x01" + bytes(9)
    header = ELF_HEADER.pack(
        ident,
        2,
        243,
        1,
        entry,
        ELF_HEADER.size,
        0,
        elf_flags,
        ELF_HEADER.size,
        PROGRAM_HEADER.size,
        segments,
        0,
        0,
        0,
    )
    program_headers = [
        PROGRAM_HEADER.pack(
            1,
            code_offset,
            CODE_ADDRESS,
            CODE_ADDRESS,
            len(code),
            len(code),
            code_flags,
            4,
        )
    ]
    if segments == 2:
        data_address = CODE_ADDRESS + 4 if overlap else DATA_ADDRESS
        memory_size = len(data) if data_memory_size is None else data_memory_size
        program_headers.append(
            PROGRAM_HEADER.pack(
                1,
                data_offset,
                data_address,
                data_address,
                len(data),
                memory_size,
                data_flags,
                4,
            )
        )
    elf = bytearray(header + b"".join(program_headers))
    elf.extend(bytes(code_offset - len(elf)))
    elf.extend(code)
    if segments == 2:
        elf.extend(bytes(data_offset - len(elf)))
        elf.extend(data)
    return bytes(elf)


def exit_elf(code: int = 0) -> bytes:
    return make_elf(words(i_type(code, 0, 0, 10), 0x00000073))


class LoaderTests(unittest.TestCase):
    @staticmethod
    def with_empty_load(
        elf: bytes,
        *,
        offset: int | None = None,
        flags: int = 0xFFFF_FFFF,
        alignment: int = 1,
    ) -> bytes:
        output = bytearray(elf)
        phoff = ELF_HEADER.size
        output[44:46] = struct.pack("<H", 2)
        empty = PROGRAM_HEADER.pack(
            1,
            len(elf) if offset is None else offset,
            0xFFFF_FFFF,
            0xFFFF_FFFF,
            0,
            0,
            flags,
            alignment,
        )
        output[phoff + PROGRAM_HEADER.size : phoff + 2 * PROGRAM_HEADER.size] = empty
        return bytes(output)

    def test_bss_is_zero_and_resets(self) -> None:
        image = load_elf(
            make_elf(words(0x00000073), data=b"abc", data_memory_size=4096)
        )
        first = Machine(image, b"", 1024)
        self.assertEqual(first.memory.inspect(DATA_ADDRESS, 8), b"abc\0\0\0\0\0")
        first.memory.store_checked(
            DATA_ADDRESS + 4, 1, 99, "StoreAccessFault", CODE_ADDRESS
        )
        second = Machine(image, b"", 1024)
        self.assertEqual(second.memory.inspect(DATA_ADDRESS, 8), b"abc\0\0\0\0\0")

    def test_rejects_overlap_wx_and_bad_entry(self) -> None:
        with self.assertRaisesRegex(ElfError, "overlap"):
            load_elf(make_elf(words(0x73, 0x73), data=b"x", overlap=True))
        with self.assertRaisesRegex(ElfError, "writable executable"):
            load_elf(make_elf(words(0x73), code_flags=7))
        with self.assertRaisesRegex(ElfError, "entry point"):
            load_elf(make_elf(words(0x73), entry=DATA_ADDRESS))
        with self.assertRaisesRegex(ElfError, "not RO, RW, or RX"):
            load_elf(make_elf(words(0x73), code_flags=1))
        with self.assertRaisesRegex(ElfError, "unsupported RISC-V flags"):
            load_elf(make_elf(words(0x73), elf_flags=0x10))

    def test_empty_load_is_structurally_checked_then_ignored(self) -> None:
        valid = exit_elf()
        load_elf(self.with_empty_load(valid))
        with self.assertRaisesRegex(ElfError, "file range"):
            load_elf(self.with_empty_load(valid, offset=len(valid) + 1))
        with self.assertRaisesRegex(ElfError, "power of two"):
            load_elf(self.with_empty_load(valid, alignment=3))

    def test_executable_loads_are_four_byte_granular(self) -> None:
        valid = bytearray(exit_elf())
        phoff = ELF_HEADER.size
        valid[phoff + 16 : phoff + 20] = struct.pack("<I", 7)
        with self.assertRaisesRegex(ElfError, "4-byte granular"):
            load_elf(bytes(valid))

    def test_rejects_inconsistent_section_header_metadata(self) -> None:
        nonempty_zero_offset = bytearray(exit_elf())
        nonempty_zero_offset[46:52] = struct.pack("<HHH", SECTION_HEADER.size, 1, 0)
        with self.assertRaisesRegex(ElfError, "zero offset"):
            load_elf(bytes(nonempty_zero_offset))

        no_sections_with_name_index = bytearray(exit_elf())
        no_sections_with_name_index[50:52] = struct.pack("<H", 1)
        with self.assertRaisesRegex(ElfError, "requires section headers"):
            load_elf(bytes(no_sections_with_name_index))


class ExecutionTests(unittest.TestCase):
    def run_program(
        self, instructions, *, input_data=b"", output_limit=1024, limit=100
    ):
        image = load_elf(make_elf(words(*instructions)))
        machine = Machine(image, input_data, output_limit)
        result = machine.run(limit)
        return machine, result

    def test_exit_and_instruction_retirement(self) -> None:
        machine, result = self.run_program([i_type(7, 0, 0, 10), 0x00000073])
        self.assertEqual(result["status"], "exit")
        self.assertEqual(result["exit_code"], 7)
        self.assertEqual(result["retired_instructions"], 2)
        self.assertEqual(machine.pc, CODE_ADDRESS + 4)

    def test_write_output_uses_input_mapping(self) -> None:
        instructions = [
            i_type(1, 0, 0, 17),
            0x00000073,
            i_type(0, 0, 0, 17),
            0x00000073,
        ]
        machine, result = self.run_program(instructions, input_data=b"hello")
        self.assertEqual(bytes(machine.output), b"hello")
        self.assertEqual(result["status"], "exit")
        self.assertEqual(result["exit_code"], 5)
        self.assertEqual(result["retired_instructions"], 4)

    def test_m_extension_division_corner_cases(self) -> None:
        instructions = [
            u_type(0x80000, 1),
            i_type(-1, 0, 0, 2),
            r_type(1, 2, 1, 4, 3),  # DIV min / -1
            r_type(1, 2, 1, 6, 4),  # REM min / -1
            r_type(1, 0, 1, 4, 5),  # DIV by zero
            r_type(1, 0, 1, 6, 6),  # REM by zero
            0x00000073,
        ]
        machine, result = self.run_program(instructions)
        self.assertEqual(result["status"], "exit")
        self.assertEqual(machine.registers[3], 0x80000000)
        self.assertEqual(machine.registers[4], 0)
        self.assertEqual(machine.registers[5], 0xFFFFFFFF)
        self.assertEqual(machine.registers[6], 0x80000000)

    def test_illegal_instruction_is_precise(self) -> None:
        illegal_slli = i_type(0x20, 0, 1, 6)
        machine, result = self.run_program([i_type(9, 0, 0, 5), illegal_slli])
        self.assertEqual(result["status"], "trap")
        self.assertEqual(
            result["trap"],
            {
                "cause": "IllegalInstruction",
                "pc": CODE_ADDRESS + 4,
                "value": illegal_slli,
            },
        )
        self.assertEqual(result["retired_instructions"], 1)
        self.assertEqual(machine.registers[5], 9)
        self.assertEqual(machine.registers[6], 0)
        self.assertEqual(machine.pc, CODE_ADDRESS + 4)

    def test_store_fault_and_alignment_priority_are_atomic(self) -> None:
        instructions = [
            u_type(0x03000, 1),
            i_type(0x55, 0, 0, 2),
            s_type(1, 2, 1, 1),  # SH to odd address in read-only input.
        ]
        machine, result = self.run_program(instructions, input_data=b"abcdef")
        self.assertEqual(result["trap"]["cause"], "StoreAddressMisaligned")
        self.assertEqual(result["trap"]["value"], 0x03000001)
        self.assertEqual(machine.memory.inspect(0x03000000, 6), b"abcdef")
        self.assertEqual(result["retired_instructions"], 2)

    def test_output_limit_is_atomic(self) -> None:
        instructions = [i_type(1, 0, 0, 17), 0x00000073]
        machine, result = self.run_program(
            instructions, input_data=b"abc", output_limit=2
        )
        self.assertEqual(result["trap"]["cause"], "OutputLimitExceeded")
        self.assertEqual(result["trap"]["value"], 3)
        self.assertEqual(machine.output, b"")
        self.assertEqual(machine.registers[10], 0x03000000)
        self.assertEqual(result["retired_instructions"], 1)

    def test_instruction_limit_is_checked_before_fetch(self) -> None:
        machine, result = self.run_program([i_type(1, 0, 0, 5)], limit=0)
        self.assertEqual(result["status"], "resource_failure")
        self.assertEqual(result["resource_failure"], {"cause": "InstructionLimit"})
        self.assertEqual(machine.pc, CODE_ADDRESS)
        self.assertEqual(machine.registers[5], 0)

    def test_all_funct3_zero_fence_encodings_are_noops(self) -> None:
        # Nonzero fm, pred/succ, rs1, and rd with funct3 still exactly zero.
        unusual_fence = (0xFFF << 20) | (31 << 15) | (31 << 7) | 0x0F
        _, result = self.run_program([unusual_fence, 0x00000073])
        self.assertEqual(result["status"], "exit")
        self.assertEqual(result["retired_instructions"], 2)

        fence_i = 0x0000100F
        _, result = self.run_program([fence_i])
        self.assertEqual(result["trap"]["cause"], "IllegalInstruction")
        self.assertEqual(result["trap"]["value"], fence_i)

    def test_integer_alu_decode_matrix(self) -> None:
        cases = [
            (i_type(1, 1, 0, 3), {1: 0xFFFFFFFF}, 0),  # ADDI
            (i_type(-1, 1, 2, 3), {1: 0x80000000}, 1),  # SLTI
            (i_type(-1, 1, 3, 3), {1: 0xFFFFFFFE}, 1),  # SLTIU
            (i_type(-1, 1, 4, 3), {1: 0xA5A5A5A5}, 0x5A5A5A5A),
            (i_type(0x55, 1, 6, 3), {1: 0xAA00}, 0xAA55),
            (i_type(0x55, 1, 7, 3), {1: 0xAAFF}, 0x55),
            (i_type(31, 1, 1, 3), {1: 1}, 0x80000000),  # SLLI
            (i_type(31, 1, 5, 3), {1: 0x80000000}, 1),  # SRLI
            (i_type(0x400 | 31, 1, 5, 3), {1: 0x80000000}, 0xFFFFFFFF),
            (r_type(0, 2, 1, 0, 3), {1: 0xFFFFFFFF, 2: 2}, 1),  # ADD
            (r_type(0x20, 2, 1, 0, 3), {1: 1, 2: 2}, 0xFFFFFFFF),
            (r_type(0, 2, 1, 1, 3), {1: 1, 2: 33}, 2),  # SLL
            (r_type(0, 2, 1, 2, 3), {1: 0x80000000, 2: 0}, 1),
            (r_type(0, 2, 1, 3, 3), {1: 0x80000000, 2: 0}, 0),
            (r_type(0, 2, 1, 4, 3), {1: 0xA5, 2: 0x3C}, 0x99),
            (r_type(0, 2, 1, 5, 3), {1: 0x80000000, 2: 31}, 1),
            (r_type(0x20, 2, 1, 5, 3), {1: 0x80000000, 2: 31}, 0xFFFFFFFF),
            (r_type(0, 2, 1, 6, 3), {1: 0xA0, 2: 0x0A}, 0xAA),
            (r_type(0, 2, 1, 7, 3), {1: 0xAF, 2: 0x5A}, 0x0A),
        ]
        image_cache = {}
        for instruction, register_values, expected in cases:
            with self.subTest(instruction=f"0x{instruction:08x}"):
                image = image_cache.get(instruction)
                if image is None:
                    image = load_elf(make_elf(words(instruction, 0x00000073)))
                    image_cache[instruction] = image
                machine = Machine(image, b"", 1024)
                for register, value in register_values.items():
                    machine.registers[register] = value
                machine._step()
                self.assertEqual(machine.registers[3], expected)
                self.assertEqual(machine.pc, CODE_ADDRESS + 4)

    def test_multiply_and_unsigned_divide_matrix(self) -> None:
        cases = [
            (0, 0xFFFFFFFF, 2, 0xFFFFFFFE),  # MUL
            (1, 0xFFFFFFFF, 2, 0xFFFFFFFF),  # MULH
            (2, 0xFFFFFFFF, 2, 0xFFFFFFFF),  # MULHSU
            (3, 0xFFFFFFFF, 2, 1),  # MULHU
            (5, 10, 3, 3),  # DIVU
            (7, 10, 3, 1),  # REMU
            (5, 10, 0, 0xFFFFFFFF),
            (7, 10, 0, 10),
        ]
        for funct3, left, right, expected in cases:
            instruction = r_type(1, 2, 1, funct3, 3)
            with self.subTest(funct3=funct3, left=left, right=right):
                machine = Machine(load_elf(make_elf(words(instruction))), b"", 1024)
                machine.registers[1] = left
                machine.registers[2] = right
                machine._step()
                self.assertEqual(machine.registers[3], expected)

    def test_load_store_widths_and_sign_extension(self) -> None:
        data = b"\x80\x7f\x00\x80\x78\x56\x34\x12"
        load_cases = [
            (0, 0, 0xFFFFFF80),  # LB
            (4, 0, 0x80),  # LBU
            (1, 2, 0xFFFF8000),  # LH
            (5, 2, 0x8000),  # LHU
            (2, 4, 0x12345678),  # LW
        ]
        for funct3, offset, expected in load_cases:
            instruction = i_type(offset, 1, funct3, 3, opcode=0x03)
            image = load_elf(make_elf(words(instruction), data=data))
            machine = Machine(image, b"", 1024)
            machine.registers[1] = DATA_ADDRESS
            machine._step()
            self.assertEqual(machine.registers[3], expected)

        store_cases = [
            (0, 1, b"\x78"),
            (1, 2, b"\x78\x56"),
            (2, 4, b"\x78\x56\x34\x12"),
        ]
        for funct3, size, expected in store_cases:
            instruction = s_type(0, 2, 1, funct3)
            image = load_elf(
                make_elf(words(instruction), data=b"", data_memory_size=4096)
            )
            machine = Machine(image, b"", 1024)
            machine.registers[1] = DATA_ADDRESS
            machine.registers[2] = 0x12345678
            machine._step()
            self.assertEqual(machine.memory.inspect(DATA_ADDRESS, size), expected)

    def test_branches_jumps_and_upper_immediates(self) -> None:
        branch_cases = [
            (0, 5, 5),  # BEQ
            (1, 5, 6),  # BNE
            (4, 0xFFFFFFFF, 0),  # BLT
            (5, 0, 0xFFFFFFFF),  # BGE
            (6, 0, 0xFFFFFFFF),  # BLTU
            (7, 0xFFFFFFFF, 0),  # BGEU
        ]
        for funct3, left, right in branch_cases:
            instruction = b_type(8, 2, 1, funct3)
            machine = Machine(load_elf(make_elf(words(instruction))), b"", 1024)
            machine.registers[1] = left
            machine.registers[2] = right
            machine._step()
            self.assertEqual(machine.pc, CODE_ADDRESS + 8)

        machine = Machine(load_elf(make_elf(words(b_type(8, 2, 1, 0)))), b"", 1024)
        machine.registers[1] = 1
        machine.registers[2] = 2
        machine._step()
        self.assertEqual(machine.pc, CODE_ADDRESS + 4)

        machine = Machine(load_elf(make_elf(words(j_type(8, 3)))), b"", 1024)
        machine._step()
        self.assertEqual(machine.pc, CODE_ADDRESS + 8)
        self.assertEqual(machine.registers[3], CODE_ADDRESS + 4)

        jalr = i_type(0, 1, 0, 3, opcode=0x67)
        machine = Machine(load_elf(make_elf(words(jalr))), b"", 1024)
        machine.registers[1] = CODE_ADDRESS + 9
        machine._step()
        self.assertEqual(machine.pc, CODE_ADDRESS + 8)
        self.assertEqual(machine.registers[3], CODE_ADDRESS + 4)

        instructions = [u_type(0xABCDE, 3), u_type(1, 4, opcode=0x17)]
        machine, result = self.run_program(instructions, limit=2)
        self.assertEqual(result["status"], "resource_failure")
        self.assertEqual(machine.registers[3], 0xABCDE000)
        self.assertEqual(machine.registers[4], CODE_ADDRESS + 4 + 0x1000)

    def test_misaligned_jump_and_ebreak_are_precise(self) -> None:
        machine, result = self.run_program([j_type(2, 3)])
        self.assertEqual(result["trap"]["cause"], "InstructionAddressMisaligned")
        self.assertEqual(result["trap"]["value"], CODE_ADDRESS + 2)
        self.assertEqual(machine.registers[3], 0)
        self.assertEqual(machine.pc, CODE_ADDRESS)

        machine, result = self.run_program([0x00100073])
        self.assertEqual(
            result["trap"], {"cause": "Breakpoint", "pc": CODE_ADDRESS, "value": 0}
        )
        self.assertEqual(result["retired_instructions"], 0)


def frame(opcode: int, request_id: int, payload: bytes = b"") -> bytes:
    return HEADER.pack(MAGIC, VERSION, opcode, 0, request_id, len(payload)) + payload


def parse_frames(data: bytes):
    frames = []
    offset = 0
    while offset < len(data):
        header = HEADER.unpack_from(data, offset)
        offset += HEADER.size
        length = header[-1]
        payload = data[offset : offset + length]
        offset += length
        frames.append((header, payload))
    return frames


class ProtocolTests(unittest.TestCase):
    def test_repeated_runs_reset_all_guest_state(self) -> None:
        elf = exit_elf(4)
        run_payload = RUN_HEADER.pack(100, 1024, 0)
        requests = b"".join(
            [
                frame(OP_LOAD, 1, elf),
                frame(OP_RUN, 2, run_payload),
                frame(OP_RESET, 3),
                frame(OP_RUN, 4, run_payload),
                frame(OP_UNLOAD, 5),
                frame(OP_SHUTDOWN, 6),
            ]
        )
        output = io.BytesIO()
        self.assertEqual(serve(io.BytesIO(requests), output), 0)
        responses = parse_frames(output.getvalue())
        self.assertEqual(responses[0][0][2:], (0x80, STATUS_OK, 0, 0))
        self.assertEqual([response[0][3] for response in responses], [0] * 7)

        run_results = []
        for index in (2, 4):
            payload = responses[index][1]
            json_length, output_length = RUN_RESPONSE_HEADER.unpack_from(payload)
            encoded = payload[
                RUN_RESPONSE_HEADER.size : RUN_RESPONSE_HEADER.size + json_length
            ]
            self.assertEqual(output_length, 0)
            run_results.append(json.loads(encoded))
        self.assertEqual(run_results[0], run_results[1])
        self.assertEqual(run_results[0]["exit_code"], 4)

    def test_empty_load_and_bad_magic_have_fixed_errors(self) -> None:
        requests = frame(OP_LOAD, 1) + frame(OP_SHUTDOWN, 2)
        output = io.BytesIO()
        self.assertEqual(serve(io.BytesIO(requests), output), 0)
        responses = parse_frames(output.getvalue())
        self.assertEqual(responses[1][0][3], STATUS_INVALID_PAYLOAD)
        self.assertEqual(responses[1][1], b"invalid payload")

        malformed = HEADER.pack(b"BAD!", VERSION, OP_LOAD, 0, 7, 0)
        output = io.BytesIO()
        self.assertEqual(serve(io.BytesIO(malformed), output), 2)
        responses = parse_frames(output.getvalue())
        self.assertEqual(responses[1][0][3], STATUS_MALFORMED_FRAME)
        self.assertEqual(responses[1][1], b"malformed frame")

    def test_run_payload_is_validated_before_empty_state(self) -> None:
        output = io.BytesIO()
        requests = frame(OP_RUN, 9) + frame(OP_SHUTDOWN, 10)
        self.assertEqual(serve(io.BytesIO(requests), output), 0)
        responses = parse_frames(output.getvalue())
        self.assertEqual(responses[1][0][3], STATUS_INVALID_PAYLOAD)
        self.assertEqual(responses[1][1], b"invalid payload")

    def test_correlated_truncated_headers_receive_fatal_error(self) -> None:
        complete = frame(OP_RESET, 0x12345678)
        for cut in range(12, HEADER.size):
            with self.subTest(cut=cut):
                output = io.BytesIO()
                self.assertEqual(serve(io.BytesIO(complete[:cut]), output), 2)
                responses = parse_frames(output.getvalue())
                self.assertEqual(len(responses), 2)
                self.assertEqual(responses[1][0][2], OP_RESET | 0x80)
                self.assertEqual(responses[1][0][4], 0x12345678)
                self.assertEqual(responses[1][0][3], STATUS_MALFORMED_FRAME)


class CliTests(unittest.TestCase):
    def test_one_shot_and_diagnostic_state(self) -> None:
        instructions = [
            i_type(1, 0, 0, 17),
            0x00000073,
            i_type(0, 0, 0, 17),
            0x00000073,
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            elf_path = root / "program.elf"
            input_path = root / "input.bin"
            output_path = root / "output.bin"
            result_path = root / "result.json"
            state_path = root / "state.json"
            elf_path.write_bytes(make_elf(words(*instructions)))
            input_path.write_bytes(b"hello")
            return_code = main(
                [
                    "run",
                    "--elf",
                    str(elf_path),
                    "--input",
                    str(input_path),
                    "--output",
                    str(output_path),
                    "--result",
                    str(result_path),
                    "--state",
                    str(state_path),
                    "--inspect",
                    "0x03000000:5",
                ]
            )
            self.assertEqual(return_code, 0)
            self.assertEqual(output_path.read_bytes(), b"hello")
            result = json.loads(result_path.read_bytes())
            self.assertEqual(result["status"], "exit")
            state = json.loads(state_path.read_bytes())
            self.assertEqual(state["pc"], CODE_ADDRESS + 12)
            self.assertEqual(state["registers"][0], 0)
            self.assertEqual(state["memory"][0]["data_base64"], "aGVsbG8=")
            self.assertEqual(
                state_path.read_bytes(),
                json.dumps(
                    state,
                    ensure_ascii=True,
                    allow_nan=False,
                    separators=(",", ":"),
                ).encode("utf-8"),
            )

    def test_invalid_inspection_does_not_replace_outputs(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            elf_path = root / "program.elf"
            input_path = root / "input.bin"
            output_path = root / "output.bin"
            result_path = root / "result.json"
            state_path = root / "state.json"
            elf_path.write_bytes(exit_elf())
            input_path.write_bytes(b"")
            for path in (output_path, result_path, state_path):
                path.write_bytes(b"sentinel")
            return_code = main(
                [
                    "run",
                    "--elf",
                    str(elf_path),
                    "--input",
                    str(input_path),
                    "--output",
                    str(output_path),
                    "--result",
                    str(result_path),
                    "--state",
                    str(state_path),
                    "--inspect",
                    "0:1",
                ]
            )
            self.assertNotEqual(return_code, 0)
            for path in (output_path, result_path, state_path):
                self.assertEqual(path.read_bytes(), b"sentinel")


if __name__ == "__main__":
    unittest.main()
