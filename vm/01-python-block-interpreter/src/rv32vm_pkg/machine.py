"""Cached straight-line-block RV32IM interpreter."""

from __future__ import annotations

from .constants import (
    INPUT_START,
    MASK32,
    PERM_EXEC,
    PERM_READ,
    STACK_END,
)
from .elf import load_elf
from .errors import GuestTrap

_MAX_BLOCK_INSTRUCTIONS = 64
_KNOWN_OPCODES = frozenset(
    (0x03, 0x0F, 0x13, 0x17, 0x23, 0x33, 0x37, 0x63, 0x67, 0x6F, 0x73)
)
_BLOCK_TERMINATORS = frozenset((0x63, 0x67, 0x6F, 0x73))
# instruction, opcode, rd, funct3, rs1, rs2, funct7, immediate
_DecodedInstruction = tuple[int, int, int, int, int, int, int, int]
_DecodedBlock = tuple[_DecodedInstruction, ...]
_BlockCache = dict[int, _DecodedBlock]


def _signed(value: int) -> int:
    return value if value < 0x8000_0000 else value - 0x1_0000_0000


def _sign_extend(value: int, bits: int) -> int:
    sign = 1 << (bits - 1)
    return (value ^ sign) - sign


def _decode(instruction: int) -> _DecodedInstruction:
    """Decode fields that are stable for immutable executable memory."""

    opcode = instruction & 0x7F
    rd = (instruction >> 7) & 0x1F
    funct3 = (instruction >> 12) & 7
    rs1 = (instruction >> 15) & 0x1F
    rs2 = (instruction >> 20) & 0x1F
    funct7 = instruction >> 25
    if opcode in (0x03, 0x13, 0x67):
        immediate = _sign_extend(instruction >> 20, 12)
    elif opcode == 0x23:
        encoded = ((instruction >> 7) & 0x1F) | (((instruction >> 25) & 0x7F) << 5)
        immediate = _sign_extend(encoded, 12)
    elif opcode == 0x63:
        encoded = (
            (((instruction >> 31) & 1) << 12)
            | (((instruction >> 7) & 1) << 11)
            | (((instruction >> 25) & 0x3F) << 5)
            | (((instruction >> 8) & 0xF) << 1)
        )
        immediate = _sign_extend(encoded, 13)
    elif opcode == 0x6F:
        encoded = (
            ((instruction >> 31) << 20)
            | (((instruction >> 12) & 0xFF) << 12)
            | (((instruction >> 20) & 1) << 11)
            | (((instruction >> 21) & 0x3FF) << 1)
        )
        immediate = _sign_extend(encoded, 21)
    else:
        immediate = 0
    return (instruction, opcode, rd, funct3, rs1, rs2, funct7, immediate)


def _translate_block(memory, start_pc: int) -> _DecodedBlock:
    """Decode one bounded straight-line block without observing mutable state."""

    block: list[tuple[int, ...]] = []
    pc = start_pc
    while len(block) < _MAX_BLOCK_INSTRUCTIONS:
        try:
            if pc & 3:
                raise GuestTrap("InstructionAddressMisaligned", pc, pc)
            memory.check(pc, 4, PERM_EXEC, "InstructionAccessFault", pc)
        except GuestTrap:
            # A fault beyond a valid prefix must occur only after that prefix
            # commits. A fault at the block entry is immediately observable.
            if not block:
                raise
            break
        decoded = _decode(memory.load_u32(pc))
        block.append(decoded)
        opcode = decoded[1]
        if opcode in _BLOCK_TERMINATORS or opcode not in _KNOWN_OPCODES:
            break
        pc = (pc + 4) & MASK32
    return tuple(block)


def _result(
    status: str,
    retired: int,
    output_length: int,
    *,
    exit_code: int | None = None,
    trap: dict[str, int | str] | None = None,
    resource_failure: dict[str, str] | None = None,
) -> dict:
    # Insertion order is the normative compact wire/file order.
    return {
        "schema_version": 1,
        "status": status,
        "exit_code": exit_code,
        "trap": trap,
        "resource_failure": resource_failure,
        "retired_instructions": retired,
        "output_length": output_length,
    }


class LoadedProgram:
    """A loaded ELF image with decoded blocks cached by starting PC."""

    __slots__ = ("block_cache", "image")

    def __init__(self, elf_data: bytes) -> None:
        self.image = load_elf(elf_data)
        self.block_cache: _BlockCache = {}

    def new_machine(self, input_data: bytes, output_limit: int) -> Machine:
        return Machine(self.image, input_data, output_limit, self.block_cache)


class Machine:
    """One freshly initialized guest run sharing only its image's decode cache."""

    __slots__ = (
        "block_cache",
        "memory",
        "output",
        "output_limit",
        "pc",
        "registers",
        "retired_instructions",
    )

    def __init__(
        self,
        image,
        input_data: bytes,
        output_limit: int,
        block_cache: _BlockCache | None = None,
    ) -> None:
        self.registers = [0] * 32
        self.registers[2] = STACK_END
        self.registers[10] = INPUT_START
        self.registers[11] = len(input_data)
        self.pc = image.entry
        self.memory = image.new_memory(input_data)
        self.output = bytearray()
        self.retired_instructions = 0
        self.output_limit = output_limit
        self.block_cache = {} if block_cache is None else block_cache

    def run(self, instruction_limit: int) -> dict:
        registers = self.registers
        memory = self.memory
        output = self.output
        output_limit = self.output_limit
        block_cache = self.block_cache
        pc = self.pc
        retired = self.retired_instructions

        try:
            while True:
                if retired >= instruction_limit:
                    self.pc = pc
                    self.retired_instructions = retired
                    return _result(
                        "resource_failure",
                        retired,
                        len(output),
                        resource_failure={"cause": "InstructionLimit"},
                    )

                block = block_cache.get(pc)
                if block is None:
                    block = _translate_block(memory, pc)
                    block_cache[pc] = block

                for decoded in block:
                    # Limits apply to each instruction, not each cached block.
                    if retired >= instruction_limit:
                        self.pc = pc
                        self.retired_instructions = retired
                        return _result(
                            "resource_failure",
                            retired,
                            len(output),
                            resource_failure={"cause": "InstructionLimit"},
                        )

                    (
                        instruction,
                        opcode,
                        rd,
                        funct3,
                        rs1,
                        rs2,
                        funct7,
                        immediate,
                    ) = decoded
                    next_pc = (pc + 4) & MASK32

                    if opcode == 0x37:  # LUI
                        if rd:
                            registers[rd] = instruction & 0xFFFFF000
                        pc = next_pc

                    elif opcode == 0x17:  # AUIPC
                        if rd:
                            registers[rd] = (pc + (instruction & 0xFFFFF000)) & MASK32
                        pc = next_pc

                    elif opcode == 0x6F:  # JAL
                        target = (pc + immediate) & MASK32
                        if target & 3:
                            raise GuestTrap("InstructionAddressMisaligned", pc, target)
                        if rd:
                            registers[rd] = next_pc
                        pc = target

                    elif opcode == 0x67:  # JALR
                        if funct3 != 0:
                            raise GuestTrap("IllegalInstruction", pc, instruction)
                        target = ((registers[rs1] + immediate) & MASK32) & ~1
                        if target & 3:
                            raise GuestTrap("InstructionAddressMisaligned", pc, target)
                        if rd:
                            registers[rd] = next_pc
                        pc = target

                    elif opcode == 0x63:  # Conditional branches
                        if funct3 not in (0, 1, 4, 5, 6, 7):
                            raise GuestTrap("IllegalInstruction", pc, instruction)
                        left = registers[rs1]
                        right = registers[rs2]
                        if funct3 == 0:
                            taken = left == right
                        elif funct3 == 1:
                            taken = left != right
                        elif funct3 == 4:
                            taken = _signed(left) < _signed(right)
                        elif funct3 == 5:
                            taken = _signed(left) >= _signed(right)
                        elif funct3 == 6:
                            taken = left < right
                        else:
                            taken = left >= right
                        if taken:
                            target = (pc + immediate) & MASK32
                            if target & 3:
                                raise GuestTrap(
                                    "InstructionAddressMisaligned", pc, target
                                )
                            pc = target
                        else:
                            pc = next_pc

                    elif opcode == 0x03:  # Loads
                        if funct3 not in (0, 1, 2, 4, 5):
                            raise GuestTrap("IllegalInstruction", pc, instruction)
                        address = (registers[rs1] + immediate) & MASK32
                        if funct3 in (0, 4):
                            size = 1
                        elif funct3 in (1, 5):
                            size = 2
                        else:
                            size = 4
                        if address & (size - 1):
                            raise GuestTrap("LoadAddressMisaligned", pc, address)
                        memory.check(address, size, PERM_READ, "LoadAccessFault", pc)
                        if size == 1:
                            value = memory.load_u8(address)
                            if funct3 == 0:
                                value = _sign_extend(value, 8)
                        elif size == 2:
                            value = memory.load_u16(address)
                            if funct3 == 1:
                                value = _sign_extend(value, 16)
                        else:
                            value = memory.load_u32(address)
                        if rd:
                            registers[rd] = value & MASK32
                        pc = next_pc

                    elif opcode == 0x23:  # Stores
                        if funct3 not in (0, 1, 2):
                            raise GuestTrap("IllegalInstruction", pc, instruction)
                        address = (registers[rs1] + immediate) & MASK32
                        size = 1 << funct3
                        if address & (size - 1):
                            raise GuestTrap("StoreAddressMisaligned", pc, address)
                        memory.store_checked(
                            address,
                            size,
                            registers[rs2],
                            "StoreAccessFault",
                            pc,
                        )
                        pc = next_pc

                    elif opcode == 0x13:  # Immediate ALU
                        source = registers[rs1]
                        if funct3 == 0:
                            value = source + immediate
                        elif funct3 == 2:
                            value = int(_signed(source) < immediate)
                        elif funct3 == 3:
                            value = int(source < (immediate & MASK32))
                        elif funct3 == 4:
                            value = source ^ immediate
                        elif funct3 == 6:
                            value = source | immediate
                        elif funct3 == 7:
                            value = source & immediate
                        elif funct3 == 1:
                            if funct7 != 0:
                                raise GuestTrap("IllegalInstruction", pc, instruction)
                            value = source << rs2
                        elif funct3 == 5:
                            if funct7 == 0:
                                value = source >> rs2
                            elif funct7 == 0x20:
                                value = _signed(source) >> rs2
                            else:
                                raise GuestTrap("IllegalInstruction", pc, instruction)
                        else:
                            raise GuestTrap("IllegalInstruction", pc, instruction)
                        if rd:
                            registers[rd] = value & MASK32
                        pc = next_pc

                    elif opcode == 0x33:  # Register ALU and M extension
                        left = registers[rs1]
                        right = registers[rs2]
                        shift = right & 31
                        if funct7 == 0:
                            if funct3 == 0:
                                value = left + right
                            elif funct3 == 1:
                                value = left << shift
                            elif funct3 == 2:
                                value = int(_signed(left) < _signed(right))
                            elif funct3 == 3:
                                value = int(left < right)
                            elif funct3 == 4:
                                value = left ^ right
                            elif funct3 == 5:
                                value = left >> shift
                            elif funct3 == 6:
                                value = left | right
                            elif funct3 == 7:
                                value = left & right
                            else:
                                raise GuestTrap("IllegalInstruction", pc, instruction)
                        elif funct7 == 0x20:
                            if funct3 == 0:
                                value = left - right
                            elif funct3 == 5:
                                value = _signed(left) >> shift
                            else:
                                raise GuestTrap("IllegalInstruction", pc, instruction)
                        elif funct7 == 1:
                            if funct3 == 0:  # MUL
                                value = left * right
                            elif funct3 == 1:  # MULH
                                value = (_signed(left) * _signed(right)) >> 32
                            elif funct3 == 2:  # MULHSU
                                value = (_signed(left) * right) >> 32
                            elif funct3 == 3:  # MULHU
                                value = (left * right) >> 32
                            elif funct3 == 4:  # DIV
                                signed_left = _signed(left)
                                signed_right = _signed(right)
                                if right == 0:
                                    value = MASK32
                                elif left == 0x8000_0000 and right == MASK32:
                                    value = 0x8000_0000
                                else:
                                    quotient = abs(signed_left) // abs(signed_right)
                                    value = (
                                        -quotient
                                        if (signed_left < 0) != (signed_right < 0)
                                        else quotient
                                    )
                            elif funct3 == 5:  # DIVU
                                value = MASK32 if right == 0 else left // right
                            elif funct3 == 6:  # REM
                                signed_left = _signed(left)
                                signed_right = _signed(right)
                                if right == 0:
                                    value = left
                                elif left == 0x8000_0000 and right == MASK32:
                                    value = 0
                                else:
                                    quotient = abs(signed_left) // abs(signed_right)
                                    if (signed_left < 0) != (signed_right < 0):
                                        quotient = -quotient
                                    value = signed_left - quotient * signed_right
                            elif funct3 == 7:  # REMU
                                value = left if right == 0 else left % right
                            else:
                                raise GuestTrap("IllegalInstruction", pc, instruction)
                        else:
                            raise GuestTrap("IllegalInstruction", pc, instruction)
                        if rd:
                            registers[rd] = value & MASK32
                        pc = next_pc

                    elif opcode == 0x0F:  # FENCE is the EEI no-op
                        if funct3 != 0:
                            raise GuestTrap("IllegalInstruction", pc, instruction)
                        pc = next_pc

                    elif opcode == 0x73:  # SYSTEM
                        if instruction == 0x0010_0073:
                            raise GuestTrap("Breakpoint", pc, 0)
                        if instruction != 0x0000_0073:
                            raise GuestTrap("IllegalInstruction", pc, instruction)
                        syscall = registers[17]
                        if syscall == 0:
                            retired += 1
                            self.pc = pc
                            self.retired_instructions = retired
                            return _result(
                                "exit",
                                retired,
                                len(output),
                                exit_code=registers[10],
                            )
                        if syscall != 1:
                            raise GuestTrap("InvalidSyscall", pc, syscall)
                        address = registers[10]
                        length = registers[11]
                        if length:
                            memory.check(
                                address,
                                length,
                                PERM_READ,
                                "LoadAccessFault",
                                pc,
                            )
                            output_bytes = memory.read_unchecked(address, length)
                        else:
                            output_bytes = b""
                        resulting_length = len(output) + length
                        if resulting_length > output_limit:
                            raise GuestTrap(
                                "OutputLimitExceeded",
                                pc,
                                resulting_length & MASK32,
                            )
                        output.extend(output_bytes)
                        registers[10] = length
                        pc = next_pc

                    else:
                        raise GuestTrap("IllegalInstruction", pc, instruction)

                    retired += 1
        except GuestTrap as trap:
            self.pc = pc
            self.retired_instructions = retired
            return _result(
                "trap",
                retired,
                len(output),
                trap={"cause": trap.cause, "pc": trap.pc, "value": trap.value},
            )
