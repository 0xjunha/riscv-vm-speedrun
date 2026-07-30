"""Canonical fetch/decode/execute RV32IM interpreter."""

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

_CONTINUE = object()


def _signed(value: int) -> int:
    return value if value < 0x8000_0000 else value - 0x1_0000_0000


def _sign_extend(value: int, bits: int) -> int:
    sign = 1 << (bits - 1)
    return (value ^ sign) - sign


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
    """A loaded ELF image used to create a fresh machine for each run."""

    __slots__ = ("image",)

    def __init__(self, elf_data: bytes) -> None:
        self.image = load_elf(elf_data)

    def new_machine(self, input_data: bytes, output_limit: int) -> Machine:
        return Machine(self.image, input_data, output_limit)


class Machine:
    """One freshly initialized guest run."""

    __slots__ = (
        "memory",
        "output",
        "output_limit",
        "pc",
        "registers",
        "retired_instructions",
    )

    def __init__(self, image, input_data: bytes, output_limit: int) -> None:
        self.registers = [0] * 32
        self.registers[2] = STACK_END
        self.registers[10] = INPUT_START
        self.registers[11] = len(input_data)
        self.pc = image.entry
        self.memory = image.new_memory(input_data)
        self.output = bytearray()
        self.retired_instructions = 0
        self.output_limit = output_limit

    def run(self, instruction_limit: int) -> dict:
        while True:
            if self.retired_instructions >= instruction_limit:
                return _result(
                    "resource_failure",
                    self.retired_instructions,
                    len(self.output),
                    resource_failure={"cause": "InstructionLimit"},
                )
            try:
                exit_code = self._step()
            except GuestTrap as trap:
                return _result(
                    "trap",
                    self.retired_instructions,
                    len(self.output),
                    trap={"cause": trap.cause, "pc": trap.pc, "value": trap.value},
                )
            self.retired_instructions += 1
            if exit_code is not _CONTINUE:
                return _result(
                    "exit",
                    self.retired_instructions,
                    len(self.output),
                    exit_code=exit_code,
                )

    def _write_register(self, register: int, value: int) -> None:
        if register:
            self.registers[register] = value & MASK32

    def _illegal(self, instruction: int, pc: int) -> None:
        raise GuestTrap("IllegalInstruction", pc, instruction)

    def _step(self):
        registers = self.registers
        memory = self.memory
        pc = self.pc

        if pc & 3:
            raise GuestTrap("InstructionAddressMisaligned", pc, pc)
        memory.check(pc, 4, PERM_EXEC, "InstructionAccessFault", pc)
        instruction = memory.load_u32(pc)
        opcode = instruction & 0x7F
        rd = (instruction >> 7) & 0x1F
        funct3 = (instruction >> 12) & 7
        rs1 = (instruction >> 15) & 0x1F
        rs2 = (instruction >> 20) & 0x1F
        funct7 = instruction >> 25
        next_pc = (pc + 4) & MASK32

        # LUI
        if opcode == 0x37:
            self._write_register(rd, instruction & 0xFFFFF000)
            self.pc = next_pc
            return _CONTINUE

        # AUIPC
        if opcode == 0x17:
            self._write_register(rd, pc + (instruction & 0xFFFFF000))
            self.pc = next_pc
            return _CONTINUE

        # JAL
        if opcode == 0x6F:
            immediate = (
                ((instruction >> 31) << 20)
                | (((instruction >> 12) & 0xFF) << 12)
                | (((instruction >> 20) & 1) << 11)
                | (((instruction >> 21) & 0x3FF) << 1)
            )
            target = (pc + _sign_extend(immediate, 21)) & MASK32
            if target & 3:
                raise GuestTrap("InstructionAddressMisaligned", pc, target)
            self._write_register(rd, next_pc)
            self.pc = target
            return _CONTINUE

        # JALR
        if opcode == 0x67:
            if funct3 != 0:
                self._illegal(instruction, pc)
            immediate = _sign_extend(instruction >> 20, 12)
            target = ((registers[rs1] + immediate) & MASK32) & ~1
            if target & 3:
                raise GuestTrap("InstructionAddressMisaligned", pc, target)
            self._write_register(rd, next_pc)
            self.pc = target
            return _CONTINUE

        # Conditional branches.
        if opcode == 0x63:
            if funct3 not in (0, 1, 4, 5, 6, 7):
                self._illegal(instruction, pc)
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
                immediate = (
                    (((instruction >> 31) & 1) << 12)
                    | (((instruction >> 7) & 1) << 11)
                    | (((instruction >> 25) & 0x3F) << 5)
                    | (((instruction >> 8) & 0xF) << 1)
                )
                target = (pc + _sign_extend(immediate, 13)) & MASK32
                if target & 3:
                    raise GuestTrap("InstructionAddressMisaligned", pc, target)
                self.pc = target
            else:
                self.pc = next_pc
            return _CONTINUE

        # Loads.
        if opcode == 0x03:
            if funct3 not in (0, 1, 2, 4, 5):
                self._illegal(instruction, pc)
            address = (registers[rs1] + _sign_extend(instruction >> 20, 12)) & MASK32
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
            self._write_register(rd, value)
            self.pc = next_pc
            return _CONTINUE

        # Stores.
        if opcode == 0x23:
            if funct3 not in (0, 1, 2):
                self._illegal(instruction, pc)
            immediate = ((instruction >> 7) & 0x1F) | (
                ((instruction >> 25) & 0x7F) << 5
            )
            address = (registers[rs1] + _sign_extend(immediate, 12)) & MASK32
            size = 1 << funct3
            if address & (size - 1):
                raise GuestTrap("StoreAddressMisaligned", pc, address)
            memory.store_checked(address, size, registers[rs2], "StoreAccessFault", pc)
            self.pc = next_pc
            return _CONTINUE

        # Immediate arithmetic and logical operations.
        if opcode == 0x13:
            source = registers[rs1]
            immediate = _sign_extend(instruction >> 20, 12)
            if funct3 == 0:  # ADDI
                value = source + immediate
            elif funct3 == 2:  # SLTI
                value = int(_signed(source) < immediate)
            elif funct3 == 3:  # SLTIU
                value = int(source < (immediate & MASK32))
            elif funct3 == 4:  # XORI
                value = source ^ immediate
            elif funct3 == 6:  # ORI
                value = source | immediate
            elif funct3 == 7:  # ANDI
                value = source & immediate
            elif funct3 == 1:  # SLLI
                if funct7 != 0:
                    self._illegal(instruction, pc)
                value = source << rs2
            elif funct3 == 5:
                if funct7 == 0:  # SRLI
                    value = source >> rs2
                elif funct7 == 0x20:  # SRAI
                    value = _signed(source) >> rs2
                else:
                    self._illegal(instruction, pc)
            else:
                self._illegal(instruction, pc)
            self._write_register(rd, value)
            self.pc = next_pc
            return _CONTINUE

        # Register arithmetic, logical operations, and the M extension.
        if opcode == 0x33:
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
                    self._illegal(instruction, pc)
            elif funct7 == 0x20:
                if funct3 == 0:
                    value = left - right
                elif funct3 == 5:
                    value = _signed(left) >> shift
                else:
                    self._illegal(instruction, pc)
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
                    self._illegal(instruction, pc)
            else:
                self._illegal(instruction, pc)
            self._write_register(rd, value)
            self.pc = next_pc
            return _CONTINUE

        # Every funct3=000 FENCE encoding is an EEI no-op.  This includes
        # architectural hints and encodings whose ignored fields are nonzero.
        if opcode == 0x0F:
            if funct3 != 0:
                self._illegal(instruction, pc)
            self.pc = next_pc
            return _CONTINUE

        # Only the exact ECALL and EBREAK encodings are supported from SYSTEM.
        if opcode == 0x73:
            if instruction == 0x0010_0073:
                raise GuestTrap("Breakpoint", pc, 0)
            if instruction != 0x0000_0073:
                self._illegal(instruction, pc)
            syscall = registers[17]
            if syscall == 0:
                return registers[10]
            if syscall != 1:
                raise GuestTrap("InvalidSyscall", pc, syscall)

            address = registers[10]
            length = registers[11]
            if length:
                memory.check(address, length, PERM_READ, "LoadAccessFault", pc)
                output_bytes = memory.read_unchecked(address, length)
            else:
                output_bytes = b""
            resulting_length = len(self.output) + length
            if resulting_length > self.output_limit:
                raise GuestTrap("OutputLimitExceeded", pc, resulting_length & MASK32)
            self.output.extend(output_bytes)
            self._write_register(10, length)
            self.pc = next_pc
            return _CONTINUE

        self._illegal(instruction, pc)
