"""Models and codecs for the host-facing rv32vm interface.

Validates protocol frames, result JSON, and diagnostic-state JSON.
"""

from __future__ import annotations

import base64
import binascii
import json
import struct
from dataclasses import dataclass
from enum import IntEnum
from typing import Any

MAGIC = b"RV32"
VERSION = 1
MESSAGE_HEADER_LAYOUT = struct.Struct("<4sBBHII")  # 16-byte message prefix.
RUN_REQUEST_LAYOUT = struct.Struct("<QII")  # 16-byte RUN request prefix.
RUN_RESPONSE_LAYOUT = struct.Struct("<II")  # 8-byte RUN response prefix.
READY_OPCODE = 0x80  # Response marker and initial READY opcode.

MAX_PAYLOAD_SIZE = 8 * 1024 * 1024  # 8 MiB.
MAX_ELF_SIZE = 8 * 1024 * 1024  # 8 MiB.
MAX_INPUT_SIZE = 4 * 1024 * 1024  # 4 MiB.
MAX_INSTRUCTION_LIMIT = 1_000_000_000  # 1 billion retired instructions.
MAX_OUTPUT_LIMIT = 1024 * 1024  # 1 MiB.
DEFAULT_INSTRUCTION_LIMIT = 100_000_000
DEFAULT_OUTPUT_LIMIT = MAX_OUTPUT_LIMIT
MAX_INSPECTION_COUNT = 1024  # 1,024 requested memory ranges.
MAX_INSPECTION_BYTES = 8 * 1024 * 1024  # 8 MiB across all ranges.
ADDRESS_SPACE_SIZE = 0x0400_0000  # 64 MiB guest address space.


class Opcode(IntEnum):
    LOAD = 0x01  # Install one ELF image.
    RUN = 0x02  # Execute the installed image.
    RESET = 0x03  # Restore the installed image's initial state.
    UNLOAD = 0x04  # Remove the installed image.
    SHUTDOWN = 0x05  # Request the persistent VM process to exit.


class Status(IntEnum):
    OK = 0  # Request succeeded.
    MALFORMED_FRAME = 1  # Message prefix is not structurally valid.
    UNSUPPORTED_VERSION = 2  # Protocol version is not supported.
    UNKNOWN_OPCODE = 3  # Requested operation is unknown.
    INVALID_FLAGS = 4  # Request flags are not zero.
    FRAME_TOO_LARGE = 5  # Payload exceeds 8 MiB.
    INVALID_PAYLOAD = 6  # Operation-specific bytes are invalid.
    INVALID_STATE = 7  # Operation is invalid in the current lifecycle state.
    ELF_REJECTED = 8  # ELF does not satisfy the VM requirements.
    INTERNAL_ERROR = 9  # VM encountered an internal failure.


# Display names for response status codes.
STATUS_NAMES = {
    Status.OK: "OK",
    Status.MALFORMED_FRAME: "MalformedFrame",
    Status.UNSUPPORTED_VERSION: "UnsupportedVersion",
    Status.UNKNOWN_OPCODE: "UnknownOpcode",
    Status.INVALID_FLAGS: "InvalidFlags",
    Status.FRAME_TOO_LARGE: "FrameTooLarge",
    Status.INVALID_PAYLOAD: "InvalidPayload",
    Status.INVALID_STATE: "InvalidState",
    Status.ELF_REJECTED: "ElfRejected",
    Status.INTERNAL_ERROR: "InternalError",
}

# Exact bytes required in non-OK responses.
ERROR_MESSAGES = {
    Status.MALFORMED_FRAME: b"malformed frame",
    Status.UNSUPPORTED_VERSION: b"unsupported version",
    Status.UNKNOWN_OPCODE: b"unknown opcode",
    Status.INVALID_FLAGS: b"invalid flags",
    Status.FRAME_TOO_LARGE: b"frame too large",
    Status.INVALID_PAYLOAD: b"invalid payload",
    Status.INVALID_STATE: b"invalid state",
    Status.ELF_REJECTED: b"ELF rejected",
    Status.INTERNAL_ERROR: b"internal error",
}

# Trap names allowed in result JSON.
TRAP_CAUSES = frozenset(
    {
        "InstructionAddressMisaligned",
        "InstructionAccessFault",
        "IllegalInstruction",
        "Breakpoint",
        "LoadAddressMisaligned",
        "LoadAccessFault",
        "StoreAddressMisaligned",
        "StoreAccessFault",
        "InvalidSyscall",
        "OutputLimitExceeded",
    }
)

# Required result JSON keys in their required order.
RESULT_KEYS = (
    "schema_version",
    "status",
    "exit_code",
    "trap",
    "resource_failure",
    "retired_instructions",
    "output_length",
)

# Required state JSON keys in their required order.
STATE_KEYS = (
    "schema_version",
    "pc",
    "registers",
    "memory",
    "retired_instructions",
    "output_length",
)


class ProtocolError(ValueError):
    """The VM emitted data that does not conform to the interface."""


class ProtocolResponseError(ProtocolError):
    """The VM returned a normative non-OK response."""

    def __init__(self, status: Status, request_id: int) -> None:
        self.status = status
        self.request_id = request_id
        self.payload = ERROR_MESSAGES[status]
        super().__init__(
            f"{STATUS_NAMES[status]} for request {request_id}: "
            f"{self.payload.decode('ascii')}"
        )


@dataclass(frozen=True)
class MessageHeader:
    magic: bytes
    version: int
    opcode: int
    status_or_flags: int
    request_id: int
    payload_length: int

    def encode(self) -> bytes:
        return MESSAGE_HEADER_LAYOUT.pack(
            self.magic,
            self.version,
            self.opcode,
            self.status_or_flags,
            self.request_id,
            self.payload_length,
        )

    @classmethod
    def decode(cls, data: bytes) -> MessageHeader:
        if len(data) != MESSAGE_HEADER_LAYOUT.size:
            raise ProtocolError(
                f"header must be exactly {MESSAGE_HEADER_LAYOUT.size} bytes"
            )
        return cls(*MESSAGE_HEADER_LAYOUT.unpack(data))


@dataclass(frozen=True)
class Trap:
    cause: str
    pc: int
    value: int


@dataclass(frozen=True)
class ResourceFailure:
    cause: str


@dataclass(frozen=True)
class RunResult:
    schema_version: int
    status: str
    exit_code: int | None
    trap: Trap | None
    resource_failure: ResourceFailure | None
    retired_instructions: int
    output_length: int

    @classmethod
    def from_json_bytes(cls, payload: bytes) -> RunResult:
        return cls.from_mapping(_load_canonical_json(payload, "result"))

    @classmethod
    def from_mapping(cls, value: Any) -> RunResult:
        _require_object(value, RESULT_KEYS, "result")
        _require_schema_version(value["schema_version"], "result")

        status = value["status"]
        if not isinstance(status, str) or status not in {
            "exit",
            "trap",
            "resource_failure",
        }:
            raise ProtocolError("result status is invalid")

        exit_code = (
            None
            if value["exit_code"] is None
            else _uint(value["exit_code"], 32, "exit_code")
        )
        trap = _parse_trap(value["trap"])
        resource_failure = _parse_resource_failure(value["resource_failure"])
        actual_variant = (
            exit_code is not None,
            trap is not None,
            resource_failure is not None,
        )
        expected_variant = {
            "exit": (True, False, False),
            "trap": (False, True, False),
            "resource_failure": (False, False, True),
        }[status]
        if actual_variant != expected_variant:
            raise ProtocolError("status and nullable result variants are inconsistent")

        return cls(
            schema_version=1,
            status=status,
            exit_code=exit_code,
            trap=trap,
            resource_failure=resource_failure,
            retired_instructions=_uint(
                value["retired_instructions"], 64, "retired_instructions"
            ),
            output_length=_uint(value["output_length"], 32, "output_length"),
        )


@dataclass(frozen=True)
class MemoryRange:
    address: int
    data: bytes


@dataclass(frozen=True)
class VMState:
    schema_version: int
    pc: int
    registers: tuple[int, ...]
    memory: tuple[MemoryRange, ...]
    retired_instructions: int
    output_length: int

    @classmethod
    def from_json_bytes(cls, payload: bytes) -> VMState:
        return cls.from_mapping(_load_canonical_json(payload, "state"))

    @classmethod
    def from_mapping(cls, value: Any) -> VMState:
        _require_object(value, STATE_KEYS, "state")
        _require_schema_version(value["schema_version"], "state")

        raw_registers = value["registers"]
        if not isinstance(raw_registers, list) or len(raw_registers) != 32:
            raise ProtocolError("state registers must contain exactly 32 entries")
        registers = tuple(
            _uint(register, 32, f"registers[{index}]")
            for index, register in enumerate(raw_registers)
        )

        raw_memory = value["memory"]
        if not isinstance(raw_memory, list):
            raise ProtocolError("state memory must be an array")
        if len(raw_memory) > MAX_INSPECTION_COUNT:
            raise ProtocolError("state memory exceeds the inspection-count limit")

        memory: list[MemoryRange] = []
        inspected_bytes = 0
        for index, item in enumerate(raw_memory):
            name = f"memory[{index}]"
            _require_object(item, ("address", "data_base64"), name)
            encoded = item["data_base64"]
            if not isinstance(encoded, str) or not encoded.isascii():
                raise ProtocolError(f"{name}.data_base64 is invalid")
            try:
                data = base64.b64decode(encoded, validate=True)
            except (ValueError, binascii.Error) as error:
                raise ProtocolError(f"{name}.data_base64 is invalid") from error
            if base64.b64encode(data).decode("ascii") != encoded:
                raise ProtocolError(f"{name}.data_base64 is not canonical")
            inspected_bytes += len(data)
            if inspected_bytes > MAX_INSPECTION_BYTES:
                raise ProtocolError("state memory exceeds the inspection-byte limit")
            address = _uint(item["address"], 32, f"{name}.address")
            if address > ADDRESS_SPACE_SIZE or address + len(data) > ADDRESS_SPACE_SIZE:
                raise ProtocolError(f"{name} is outside the guest address space")
            memory.append(MemoryRange(address, data))

        return cls(
            schema_version=1,
            pc=_uint(value["pc"], 32, "pc"),
            registers=registers,
            memory=tuple(memory),
            retired_instructions=_uint(
                value["retired_instructions"], 64, "retired_instructions"
            ),
            output_length=_uint(value["output_length"], 32, "output_length"),
        )


@dataclass(frozen=True)
class RunOutcome:
    """All data returned to the harness by one completed VM run."""

    result: RunResult
    output: bytes
    state: VMState | None = None


def encode_request(opcode: Opcode, request_id: int, payload: bytes = b"") -> bytes:
    """Encode one well-formed request frame."""

    _uint(request_id, 32, "request_id")
    if not isinstance(payload, bytes):
        raise TypeError("payload must be bytes")
    if len(payload) > MAX_PAYLOAD_SIZE:
        raise ValueError("payload exceeds the 8 MiB protocol maximum")
    return (
        MessageHeader(MAGIC, VERSION, int(opcode), 0, request_id, len(payload)).encode()
        + payload
    )


def encode_run_request(
    instruction_limit: int,
    output_limit: int,
    input_data: bytes = b"",
) -> bytes:
    """Encode the payload of a valid RUN request."""

    instruction_limit = _bounded_int(
        instruction_limit, MAX_INSTRUCTION_LIMIT, "instruction_limit"
    )
    output_limit = _bounded_int(output_limit, MAX_OUTPUT_LIMIT, "output_limit")
    if not isinstance(input_data, bytes):
        raise TypeError("input_data must be bytes")
    if len(input_data) > MAX_INPUT_SIZE:
        raise ValueError("input exceeds the 4 MiB maximum")
    return (
        RUN_REQUEST_LAYOUT.pack(instruction_limit, output_limit, len(input_data))
        + input_data
    )


def decode_ready(header: MessageHeader, payload: bytes = b"") -> None:
    """Validate the server's startup READY frame."""

    expected = MessageHeader(MAGIC, VERSION, READY_OPCODE, int(Status.OK), 0, 0)
    if header != expected or payload:
        raise ProtocolError(f"invalid READY frame: {header!r}")


def decode_response(
    header: MessageHeader,
    payload: bytes,
    *,
    opcode: Opcode,
    request_id: int,
) -> bytes:
    """Validate a response envelope and return its successful payload."""

    if len(payload) != header.payload_length:
        raise ProtocolError("response payload length does not match its header")
    if header.payload_length > MAX_PAYLOAD_SIZE:
        raise ProtocolError("response exceeds the 8 MiB protocol maximum")
    if header.magic != MAGIC or header.version != VERSION:
        raise ProtocolError("response has invalid magic or version")
    if header.opcode != (int(opcode) | READY_OPCODE):
        raise ProtocolError("response opcode does not match request")
    if header.request_id != request_id:
        raise ProtocolError("response request_id does not match request")
    try:
        status = Status(header.status_or_flags)
    except ValueError:
        raise ProtocolError("response contains an unknown status") from None
    if status is Status.OK:
        return payload
    if payload != ERROR_MESSAGES[status]:
        raise ProtocolError(
            f"non-canonical error payload for {STATUS_NAMES[status]}: {payload!r}"
        )
    raise ProtocolResponseError(status, request_id)


def decode_run_response(payload: bytes) -> RunOutcome:
    """Decode and validate the payload of a successful RUN response."""

    if len(payload) < RUN_RESPONSE_LAYOUT.size:
        raise ProtocolError("RUN response is shorter than its length prefix")
    json_length, output_length = RUN_RESPONSE_LAYOUT.unpack_from(payload)
    expected_length = RUN_RESPONSE_LAYOUT.size + json_length + output_length
    if expected_length != len(payload):
        raise ProtocolError("RUN response length prefix does not match the frame")
    json_start = RUN_RESPONSE_LAYOUT.size
    output_start = json_start + json_length
    result = RunResult.from_json_bytes(payload[json_start:output_start])
    output = payload[output_start:]
    if result.output_length != len(output):
        raise ProtocolError("result output_length does not match framed output")
    return RunOutcome(result, output)


def _parse_trap(value: Any) -> Trap | None:
    if value is None:
        return None
    _require_object(value, ("cause", "pc", "value"), "trap")
    cause = value["cause"]
    if not isinstance(cause, str) or cause not in TRAP_CAUSES:
        raise ProtocolError("unknown trap cause")
    return Trap(
        cause,
        _uint(value["pc"], 32, "trap.pc"),
        _uint(value["value"], 32, "trap.value"),
    )


def _parse_resource_failure(value: Any) -> ResourceFailure | None:
    if value is None:
        return None
    _require_object(value, ("cause",), "resource_failure")
    if value["cause"] != "InstructionLimit":
        raise ProtocolError("unknown resource failure cause")
    return ResourceFailure("InstructionLimit")


def _require_object(
    value: Any,
    keys: tuple[str, ...],
    name: str,
) -> None:
    if not isinstance(value, dict) or tuple(value) != keys:
        raise ProtocolError(
            f"{name} must contain exactly the normative keys in normative order"
        )


def _require_schema_version(value: Any, name: str) -> None:
    if type(value) is not int or value != 1:
        raise ProtocolError(f"{name} schema_version must be integer 1")


def _uint(value: Any, bits: int, name: str) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not 0 <= value < 1 << bits
    ):
        raise ProtocolError(f"{name} must be a uint{bits}")
    return value


def _bounded_int(value: Any, maximum: int, name: str) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not 0 <= value <= maximum
    ):
        raise ValueError(f"{name} must be in 0..{maximum}")
    return value


def _load_canonical_json(payload: bytes, name: str) -> Any:
    if not isinstance(payload, bytes):
        raise TypeError(f"{name} payload must be bytes")
    try:
        value = json.loads(payload.decode("utf-8"))
        canonical = json.dumps(
            value,
            ensure_ascii=True,
            allow_nan=False,
            separators=(",", ":"),
        ).encode("utf-8")
    except (
        UnicodeDecodeError,
        json.JSONDecodeError,
        RecursionError,
        ValueError,
    ) as error:
        raise ProtocolError(f"invalid {name} JSON: {error}") from error
    if payload != canonical:
        raise ProtocolError(f"{name} JSON is not in the canonical encoding")
    return value
