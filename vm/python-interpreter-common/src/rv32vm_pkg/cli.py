"""One-shot and persistent command-line interfaces."""

from __future__ import annotations

import argparse
import base64
import json
import sys
from pathlib import Path

from .constants import (
    ADDRESS_SPACE_SIZE,
    DEFAULT_INSTRUCTION_LIMIT,
    DEFAULT_OUTPUT_LIMIT,
    MAX_INPUT_LENGTH,
    MAX_INSTRUCTION_LIMIT,
    MAX_OUTPUT_LIMIT,
)
from .elf import ElfError
from .machine import LoadedProgram
from .protocol import MAX_PAYLOAD, result_bytes, serve

MAX_INSPECTION_COUNT = 1024
MAX_INSPECTION_BYTES = 8 * 1024 * 1024


def _bounded_integer(name: str, value: int, maximum: int) -> int:
    if value < 0 or value > maximum:
        raise ValueError(f"{name} must be in the range 0..{maximum}")
    return value


def _decimal_integer(value: str) -> int:
    if not value or not value.isascii() or not value.isdecimal():
        raise argparse.ArgumentTypeError("value must be an unsigned decimal integer")
    return int(value, 10)


def _parse_inspect(value: str) -> tuple[int, int]:
    def parse_unsigned(text: str) -> int:
        if text == "0":
            return 0
        if text.startswith("0x"):
            digits = text[2:]
            if not digits or any(
                character not in "0123456789abcdefABCDEF" for character in digits
            ):
                raise ValueError
            return int(digits, 16)
        if (
            not text
            or text[0] not in "123456789"
            or not text.isascii()
            or not text.isdecimal()
        ):
            raise ValueError
        return int(text, 10)

    try:
        address_text, length_text = value.split(":", 1)
        address = parse_unsigned(address_text)
        length = parse_unsigned(length_text)
    except (ValueError, TypeError) as error:
        raise ValueError("inspect range must be ADDR:LENGTH") from error
    if (
        address < 0
        or length < 0
        or address > ADDRESS_SPACE_SIZE
        or address + length > ADDRESS_SPACE_SIZE
    ):
        raise ValueError("inspect range is outside guest address space")
    return address, length


def _run_once(arguments) -> int:
    instruction_limit = _bounded_integer(
        "instruction limit", arguments.instruction_limit, MAX_INSTRUCTION_LIMIT
    )
    output_limit = _bounded_integer(
        "output limit", arguments.output_limit, MAX_OUTPUT_LIMIT
    )
    inspect_ranges = [_parse_inspect(value) for value in arguments.inspect]
    if inspect_ranges and arguments.state is None:
        raise ValueError("--inspect requires --state")
    if len(inspect_ranges) > MAX_INSPECTION_COUNT:
        raise ValueError("inspection count exceeds 1024 ranges")
    if sum(length for _, length in inspect_ranges) > MAX_INSPECTION_BYTES:
        raise ValueError("aggregate inspection length exceeds 8388608 bytes")

    elf_data = Path(arguments.elf).read_bytes()
    if len(elf_data) > MAX_PAYLOAD:
        raise ValueError("ELF exceeds the 8388608-byte protocol limit")
    input_data = Path(arguments.input).read_bytes()
    if len(input_data) > MAX_INPUT_LENGTH:
        raise ValueError("input exceeds 4194304 bytes")
    program = LoadedProgram(elf_data)
    machine = program.new_machine(input_data, output_limit)
    result = machine.run(instruction_limit)

    state_bytes = None
    if arguments.state is not None:
        memory = []
        for address, length in inspect_ranges:
            data = machine.memory.inspect(address, length)
            memory.append(
                {
                    "address": address,
                    "data_base64": base64.b64encode(data).decode("ascii"),
                }
            )
        state = {
            "schema_version": 1,
            "pc": machine.pc,
            "registers": machine.registers,
            "memory": memory,
            "retired_instructions": machine.retired_instructions,
            "output_length": len(machine.output),
        }
        state_bytes = json.dumps(
            state, ensure_ascii=True, allow_nan=False, separators=(",", ":")
        )

    Path(arguments.output).write_bytes(machine.output)
    Path(arguments.result).write_bytes(result_bytes(result))
    if state_bytes is not None:
        Path(arguments.state).write_text(state_bytes, encoding="utf-8")
    return 0


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="rv32vm", allow_abbrev=False)
    subparsers = parser.add_subparsers(dest="command", required=True)

    run_parser = subparsers.add_parser("run", allow_abbrev=False)
    run_parser.add_argument("--elf", required=True)
    run_parser.add_argument("--input", required=True)
    run_parser.add_argument("--output", required=True)
    run_parser.add_argument("--result", required=True)
    run_parser.add_argument(
        "--instruction-limit", type=_decimal_integer, default=DEFAULT_INSTRUCTION_LIMIT
    )
    run_parser.add_argument(
        "--output-limit", type=_decimal_integer, default=DEFAULT_OUTPUT_LIMIT
    )
    run_parser.add_argument("--state")
    run_parser.add_argument("--inspect", action="append", default=[])

    subparsers.add_parser("serve", allow_abbrev=False)
    return parser


def main(argv: list[str] | None = None) -> int:
    try:
        arguments = _parser().parse_args(argv)
        if arguments.command == "run":
            return _run_once(arguments)
        return serve(sys.stdin.buffer, sys.stdout.buffer)
    except (ElfError, OSError, ValueError) as error:
        print(f"rv32vm: {error}", file=sys.stderr)
        return 2
    except BrokenPipeError:
        return 2
