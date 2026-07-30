"""Shared fixed little-endian persistent rv32vm protocol."""

from __future__ import annotations

import json
import struct

from .constants import (
    MAX_INPUT_LENGTH,
    MAX_INSTRUCTION_LIMIT,
    MAX_OUTPUT_LIMIT,
)
from .elf import ElfError
from .machine import LoadedProgram

MAGIC = b"RV32"
VERSION = 1
HEADER = struct.Struct("<4sBBHII")
RUN_HEADER = struct.Struct("<QII")
RUN_RESPONSE_HEADER = struct.Struct("<II")
MAX_PAYLOAD = 8 * 1024 * 1024

OP_READY = 0x80
OP_LOAD = 1
OP_RUN = 2
OP_RESET = 3
OP_UNLOAD = 4
OP_SHUTDOWN = 5

STATUS_OK = 0
STATUS_MALFORMED_FRAME = 1
STATUS_UNSUPPORTED_VERSION = 2
STATUS_UNKNOWN_OPCODE = 3
STATUS_INVALID_FLAGS = 4
STATUS_FRAME_TOO_LARGE = 5
STATUS_INVALID_PAYLOAD = 6
STATUS_INVALID_STATE = 7
STATUS_ELF_REJECTED = 8
STATUS_INTERNAL_ERROR = 9

_STATUS_MESSAGES = {
    STATUS_MALFORMED_FRAME: "malformed frame",
    STATUS_UNSUPPORTED_VERSION: "unsupported version",
    STATUS_UNKNOWN_OPCODE: "unknown opcode",
    STATUS_INVALID_FLAGS: "invalid flags",
    STATUS_FRAME_TOO_LARGE: "frame too large",
    STATUS_INVALID_PAYLOAD: "invalid payload",
    STATUS_INVALID_STATE: "invalid state",
    STATUS_ELF_REJECTED: "ELF rejected",
    STATUS_INTERNAL_ERROR: "internal error",
}


def result_bytes(result: dict) -> bytes:
    return json.dumps(
        result, ensure_ascii=True, allow_nan=False, separators=(",", ":")
    ).encode("utf-8")


def _write_response(
    stream, opcode: int, request_id: int, status: int, payload: bytes = b""
) -> None:
    stream.write(
        HEADER.pack(MAGIC, VERSION, opcode | 0x80, status, request_id, len(payload))
    )
    if payload:
        stream.write(payload)
    stream.flush()


def _read_exact(stream, length: int) -> bytes:
    chunks = bytearray()
    while len(chunks) < length:
        chunk = stream.read(length - len(chunks))
        if not chunk:
            break
        chunks.extend(chunk)
    return bytes(chunks)


def _error(stream, opcode: int, request_id: int, status: int, message: str) -> None:
    del message
    _write_response(
        stream, opcode, request_id, status, _STATUS_MESSAGES[status].encode("utf-8")
    )


def _run_response(
    program: LoadedProgram,
    input_data: bytes,
    instruction_limit: int,
    output_limit: int,
) -> bytes:
    machine = program.new_machine(input_data, output_limit)
    encoded_result = result_bytes(machine.run(instruction_limit))
    output = bytes(machine.output)
    return (
        RUN_RESPONSE_HEADER.pack(len(encoded_result), len(output))
        + encoded_result
        + output
    )


def serve(input_stream, output_stream) -> int:
    """Serve frames until a valid SHUTDOWN request or a fatal framing error."""
    program = None
    _write_response(output_stream, OP_READY, 0, STATUS_OK)

    while True:
        raw_header = _read_exact(input_stream, HEADER.size)
        if len(raw_header) != HEADER.size:
            if len(raw_header) >= 12:
                _error(
                    output_stream,
                    raw_header[5],
                    struct.unpack_from("<I", raw_header, 8)[0],
                    STATUS_MALFORMED_FRAME,
                    "malformed frame",
                )
            return 2
        magic, version, opcode, flags, request_id, payload_length = HEADER.unpack(
            raw_header
        )
        if magic != MAGIC:
            _error(
                output_stream,
                opcode,
                request_id,
                STATUS_MALFORMED_FRAME,
                "malformed frame",
            )
            return 2
        if payload_length > MAX_PAYLOAD:
            _error(
                output_stream,
                opcode,
                request_id,
                STATUS_FRAME_TOO_LARGE,
                "payload exceeds 8388608 bytes",
            )
            return 2
        payload = _read_exact(input_stream, payload_length)
        if len(payload) != payload_length:
            _error(
                output_stream,
                opcode,
                request_id,
                STATUS_MALFORMED_FRAME,
                "malformed frame",
            )
            return 2

        if version != VERSION:
            _error(
                output_stream,
                opcode,
                request_id,
                STATUS_UNSUPPORTED_VERSION,
                "unsupported protocol version",
            )
            continue
        if flags != 0:
            _error(
                output_stream,
                opcode,
                request_id,
                STATUS_INVALID_FLAGS,
                "request flags must be zero",
            )
            continue
        if opcode not in (OP_LOAD, OP_RUN, OP_RESET, OP_UNLOAD, OP_SHUTDOWN):
            _error(
                output_stream,
                opcode,
                request_id,
                STATUS_UNKNOWN_OPCODE,
                "unknown opcode",
            )
            continue

        try:
            if opcode == OP_LOAD:
                if not payload:
                    _error(
                        output_stream,
                        opcode,
                        request_id,
                        STATUS_INVALID_PAYLOAD,
                        "invalid payload",
                    )
                    continue
                if program is not None:
                    _error(
                        output_stream,
                        opcode,
                        request_id,
                        STATUS_INVALID_STATE,
                        "an image is already loaded",
                    )
                    continue
                try:
                    program = LoadedProgram(payload)
                except ElfError as error:
                    _error(
                        output_stream,
                        opcode,
                        request_id,
                        STATUS_ELF_REJECTED,
                        str(error),
                    )
                    continue
                _write_response(output_stream, opcode, request_id, STATUS_OK)
                continue

            if opcode == OP_RUN:
                if len(payload) < RUN_HEADER.size:
                    _error(
                        output_stream,
                        opcode,
                        request_id,
                        STATUS_INVALID_PAYLOAD,
                        "RUN payload is shorter than 16 bytes",
                    )
                    continue
                instruction_limit, output_limit, input_length = RUN_HEADER.unpack_from(
                    payload
                )
                if len(payload) != RUN_HEADER.size + input_length:
                    _error(
                        output_stream,
                        opcode,
                        request_id,
                        STATUS_INVALID_PAYLOAD,
                        "RUN input length does not match payload",
                    )
                    continue
                if instruction_limit > MAX_INSTRUCTION_LIMIT:
                    _error(
                        output_stream,
                        opcode,
                        request_id,
                        STATUS_INVALID_PAYLOAD,
                        "instruction limit exceeds 100000000",
                    )
                    continue
                if output_limit > MAX_OUTPUT_LIMIT:
                    _error(
                        output_stream,
                        opcode,
                        request_id,
                        STATUS_INVALID_PAYLOAD,
                        "output limit exceeds 1048576",
                    )
                    continue
                if input_length > MAX_INPUT_LENGTH:
                    _error(
                        output_stream,
                        opcode,
                        request_id,
                        STATUS_INVALID_PAYLOAD,
                        "input length exceeds 4194304",
                    )
                    continue
                if program is None:
                    _error(
                        output_stream,
                        opcode,
                        request_id,
                        STATUS_INVALID_STATE,
                        "no image is loaded",
                    )
                    continue
                response = _run_response(
                    program,
                    payload[RUN_HEADER.size :],
                    instruction_limit,
                    output_limit,
                )
                _write_response(output_stream, opcode, request_id, STATUS_OK, response)
                continue

            if opcode == OP_RESET:
                if payload:
                    _error(
                        output_stream,
                        opcode,
                        request_id,
                        STATUS_INVALID_PAYLOAD,
                        "RESET payload must be empty",
                    )
                    continue
                if program is None:
                    _error(
                        output_stream,
                        opcode,
                        request_id,
                        STATUS_INVALID_STATE,
                        "no image is loaded",
                    )
                    continue
                _write_response(output_stream, opcode, request_id, STATUS_OK)
                continue

            if opcode == OP_UNLOAD:
                if payload:
                    _error(
                        output_stream,
                        opcode,
                        request_id,
                        STATUS_INVALID_PAYLOAD,
                        "UNLOAD payload must be empty",
                    )
                    continue
                if program is None:
                    _error(
                        output_stream,
                        opcode,
                        request_id,
                        STATUS_INVALID_STATE,
                        "no image is loaded",
                    )
                    continue
                program = None
                _write_response(output_stream, opcode, request_id, STATUS_OK)
                continue

            # SHUTDOWN is accepted in either loaded state.
            if payload:
                _error(
                    output_stream,
                    opcode,
                    request_id,
                    STATUS_INVALID_PAYLOAD,
                    "SHUTDOWN payload must be empty",
                )
                continue
            _write_response(output_stream, opcode, request_id, STATUS_OK)
            return 0
        except (MemoryError, OverflowError):
            _error(
                output_stream,
                opcode,
                request_id,
                STATUS_INTERNAL_ERROR,
                "internal server error",
            )
            return 2
        except Exception:  # noqa: BLE001
            # Do not expose host paths, exception classes, or other nondeterminism.
            _error(
                output_stream,
                opcode,
                request_id,
                STATUS_INTERNAL_ERROR,
                "internal server error",
            )
            return 2
