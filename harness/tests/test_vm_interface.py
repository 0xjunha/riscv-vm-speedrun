from __future__ import annotations

import json
from dataclasses import replace
from typing import Any

import pytest

from rv32im_harness.vm_interface import (
    MAGIC,
    MAX_INSTRUCTION_LIMIT,
    MESSAGE_HEADER_LAYOUT,
    RUN_RESPONSE_LAYOUT,
    VERSION,
    MemoryRange,
    MessageHeader,
    Opcode,
    ProtocolError,
    ProtocolResponseError,
    RunResult,
    Status,
    Trap,
    VMState,
    decode_ready,
    decode_response,
    decode_run_response,
    encode_request,
    encode_run_request,
)


def canonical(value: object) -> bytes:
    return json.dumps(value, separators=(",", ":")).encode()


def exit_result(output_length: int = 0) -> dict[str, Any]:
    return {
        "schema_version": 1,
        "status": "exit",
        "exit_code": 7,
        "trap": None,
        "resource_failure": None,
        "retired_instructions": 17,
        "output_length": output_length,
    }


def state() -> dict[str, Any]:
    return {
        "schema_version": 1,
        "pc": 0x10000,
        "registers": list(range(32)),
        "memory": [{"address": 0x20000, "data_base64": "aGVsbG8="}],
        "retired_instructions": 17,
        "output_length": 5,
    }


@pytest.mark.parametrize(
    ("value", "expected"),
    [
        (exit_result(), ("exit", 7, None, None)),
        (
            {
                **exit_result(),
                "status": "trap",
                "exit_code": None,
                "trap": {"cause": "IllegalInstruction", "pc": 0x10000, "value": 3},
            },
            ("trap", None, "IllegalInstruction", None),
        ),
        (
            {
                **exit_result(),
                "status": "resource_failure",
                "exit_code": None,
                "resource_failure": {"cause": "InstructionLimit"},
            },
            ("resource_failure", None, None, "InstructionLimit"),
        ),
    ],
)
def test_result_decodes_each_variant(
    value: dict[str, Any],
    expected: tuple[str, int | None, str | None, str | None],
) -> None:
    result = RunResult.from_json_bytes(canonical(value))
    actual = (
        result.status,
        result.exit_code,
        None if result.trap is None else result.trap.cause,
        (None if result.resource_failure is None else result.resource_failure.cause),
    )
    assert actual == expected
    assert result.retired_instructions == 17


@pytest.mark.parametrize(
    "cause",
    [
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
    ],
)
def test_result_decodes_every_trap_cause(cause: str) -> None:
    value = {
        **exit_result(),
        "status": "trap",
        "exit_code": None,
        "trap": {"cause": cause, "pc": 0x01234567, "value": 0x89ABCDEF},
    }

    trap = RunResult.from_mapping(value).trap

    assert trap == Trap(cause=cause, pc=0x01234567, value=0x89ABCDEF)


def test_state_decodes_registers_and_canonical_base64() -> None:
    decoded = VMState.from_json_bytes(canonical(state()))
    assert decoded.pc == 0x10000
    assert decoded.registers == tuple(range(32))
    assert decoded.memory == (MemoryRange(0x20000, b"hello"),)
    assert decoded.retired_instructions == 17
    assert decoded.output_length == 5


@pytest.mark.parametrize(
    "payload",
    [
        json.dumps(exit_result()).encode(),
        canonical(exit_result()) + b"\n",
        (
            b'{"status":"exit","schema_version":1,"exit_code":7,'
            b'"trap":null,"resource_failure":null,"retired_instructions":17,'
            b'"output_length":0}'
        ),
        (
            b'{"schema_version":1,"schema_version":1,"status":"exit",'
            b'"exit_code":7,"trap":null,"resource_failure":null,'
            b'"retired_instructions":17,"output_length":0}'
        ),
    ],
    ids=["whitespace", "newline", "key-order", "duplicate-key"],
)
def test_result_rejects_noncanonical_json(payload: bytes) -> None:
    with pytest.raises(ProtocolError):
        RunResult.from_json_bytes(payload)


def test_result_rejects_deeply_nested_json_as_protocol_error() -> None:
    payload = b"[" * 30_000 + b"0" + b"]" * 30_000
    assert len(payload) < 64 * 1024

    with pytest.raises(ProtocolError):
        RunResult.from_json_bytes(payload)


@pytest.mark.parametrize(
    "updates",
    [
        {"schema_version": True},
        {"status": "unknown"},
        {"exit_code": None},
        {"trap": {"cause": "UnknownTrap", "pc": 0, "value": 0}},
        {"resource_failure": {"cause": "CandidateCrash"}},
        {"retired_instructions": -1},
        {"output_length": 1 << 32},
        {"extra": None},
    ],
    ids=[
        "boolean-schema",
        "unknown-status",
        "variant-mismatch",
        "unknown-trap",
        "non-vm-resource-cause",
        "negative-retired",
        "oversized-output-length",
        "extra-key",
    ],
)
def test_result_rejects_invalid_schema(updates: dict[str, Any]) -> None:
    value = exit_result()
    value.update(updates)
    with pytest.raises(ProtocolError):
        RunResult.from_mapping(value)


@pytest.mark.parametrize(
    "updates",
    [
        {"schema_version": 1.0},
        {"pc": -1},
        {"registers": [0] * 31},
        {"registers": [False] + [0] * 31},
        {"memory": "not-an-array"},
        {"retired_instructions": 1 << 64},
        {"output_length": 1 << 32},
        {"extra": None},
    ],
    ids=[
        "float-schema",
        "negative-pc",
        "register-count",
        "boolean-register",
        "memory-type",
        "oversized-retired",
        "oversized-output-length",
        "extra-key",
    ],
)
def test_state_rejects_invalid_schema(updates: dict[str, Any]) -> None:
    value = state()
    value.update(updates)
    with pytest.raises(ProtocolError):
        VMState.from_mapping(value)


@pytest.mark.parametrize(
    "memory",
    [
        [{"data_base64": "aGVsbG8=", "address": 0x20000}],
        [{"address": 0x20000, "data_base64": "%%%"}],
        [{"address": 0x20000, "data_base64": "Zh=="}],
        [{"address": False, "data_base64": ""}],
        [{"address": 0, "data_base64": "", "extra": None}],
    ],
    ids=[
        "key-order",
        "invalid-base64",
        "noncanonical-base64",
        "boolean-address",
        "extra-key",
    ],
)
def test_state_rejects_invalid_memory_entries(memory: list[dict[str, Any]]) -> None:
    value = state()
    value["memory"] = memory
    with pytest.raises(ProtocolError):
        VMState.from_mapping(value)


@pytest.mark.parametrize(
    "memory",
    [
        [{"address": 0x0400_0001, "data_base64": ""}],
        [{"address": 0x0400_0000, "data_base64": "AA=="}],
        [{"address": 0x03FF_FFFF, "data_base64": "AAA="}],
        [{"address": 0xFFFF_FFFF, "data_base64": "AA=="}],
    ],
    ids=["address-past-end", "byte-at-end", "range-past-end", "uint32-wrap"],
)
def test_state_rejects_memory_outside_address_space(
    memory: list[dict[str, Any]],
) -> None:
    value = state()
    value["memory"] = memory
    with pytest.raises(ProtocolError, match="guest address space"):
        VMState.from_mapping(value)


def test_state_accepts_zero_length_range_at_address_space_end() -> None:
    value = state()
    value["memory"] = [{"address": 0x0400_0000, "data_base64": ""}]
    assert VMState.from_mapping(value).memory == (MemoryRange(0x0400_0000, b""),)


def test_request_and_run_payload_have_exact_wire_encoding() -> None:
    assert MESSAGE_HEADER_LAYOUT.size == 16
    assert encode_request(Opcode.RESET, 0x12345678) == bytes.fromhex(
        "52563332010300007856341200000000"
    )
    assert encode_request(Opcode.LOAD, 0x12345678, b"ELF") == bytes.fromhex(
        "52563332010100007856341203000000454c46"
    )
    assert encode_run_request(0x01020304, 0x00012344, b"abc") == bytes.fromhex(
        "04030201000000004423010003000000616263"
    )


@pytest.mark.parametrize(
    ("instruction_limit", "output_limit"),
    [(-1, 0), (MAX_INSTRUCTION_LIMIT + 1, 0), (True, 0), (0, -1), (0, 1_048_577)],
)
def test_run_request_rejects_invalid_limits(
    instruction_limit: int, output_limit: int
) -> None:
    with pytest.raises(ValueError):
        encode_run_request(instruction_limit, output_limit)


def test_ready_and_success_response_validation() -> None:
    decode_ready(MessageHeader(MAGIC, VERSION, 0x80, 0, 0, 0))
    header = MessageHeader(MAGIC, VERSION, 0x82, 0, 9, 3)
    assert decode_response(header, b"abc", opcode=Opcode.RUN, request_id=9) == b"abc"


@pytest.mark.parametrize(
    ("status_code", "payload", "expected_status"),
    [
        (1, b"malformed frame", Status.MALFORMED_FRAME),
        (2, b"unsupported version", Status.UNSUPPORTED_VERSION),
        (3, b"unknown opcode", Status.UNKNOWN_OPCODE),
        (4, b"invalid flags", Status.INVALID_FLAGS),
        (5, b"frame too large", Status.FRAME_TOO_LARGE),
        (6, b"invalid payload", Status.INVALID_PAYLOAD),
        (7, b"invalid state", Status.INVALID_STATE),
        (8, b"ELF rejected", Status.ELF_REJECTED),
        (9, b"internal error", Status.INTERNAL_ERROR),
    ],
)
def test_normative_error_responses_use_literal_codes_and_payloads(
    status_code: int,
    payload: bytes,
    expected_status: Status,
) -> None:
    header = MessageHeader(MAGIC, VERSION, 0x82, status_code, 9, len(payload))
    with pytest.raises(ProtocolResponseError) as caught:
        decode_response(header, payload, opcode=Opcode.RUN, request_id=9)
    assert caught.value.status is expected_status
    assert caught.value.payload == payload
    assert caught.value.request_id == 9


@pytest.mark.parametrize(
    "header",
    [
        MessageHeader(b"NOPE", VERSION, 0x82, 0, 9, 0),
        MessageHeader(MAGIC, 2, 0x82, 0, 9, 0),
        MessageHeader(MAGIC, VERSION, 0x83, 0, 9, 0),
        MessageHeader(MAGIC, VERSION, 0x82, 0, 10, 0),
        MessageHeader(MAGIC, VERSION, 0x82, 99, 9, 0),
        MessageHeader(MAGIC, VERSION, 0x82, 0, 9, 1),
    ],
    ids=["magic", "version", "opcode", "request-id", "status", "payload-length"],
)
def test_response_rejects_invalid_envelope(header: MessageHeader) -> None:
    with pytest.raises(ProtocolError):
        decode_response(header, b"", opcode=Opcode.RUN, request_id=9)


def test_response_rejects_noncanonical_error_payload() -> None:
    header = MessageHeader(MAGIC, VERSION, 0x82, Status.INVALID_STATE, 9, 13)
    with pytest.raises(ProtocolError):
        decode_response(header, b"INVALID STATE", opcode=Opcode.RUN, request_id=9)


def test_run_response_decodes_and_checks_all_lengths() -> None:
    output = b"hello"
    result_json = canonical(exit_result(len(output)))
    payload = (
        RUN_RESPONSE_LAYOUT.pack(len(result_json), len(output)) + result_json + output
    )
    outcome = decode_run_response(payload)
    assert outcome.output == output
    assert outcome.state is None

    for malformed in (
        payload[:-1],
        RUN_RESPONSE_LAYOUT.pack(len(result_json), 0) + result_json + output,
        RUN_RESPONSE_LAYOUT.pack(len(result_json), len(output))
        + canonical(exit_result())
        + output,
    ):
        with pytest.raises(ProtocolError):
            decode_run_response(malformed)


def test_header_round_trip_requires_exact_size() -> None:
    header = MessageHeader(MAGIC, VERSION, Opcode.RUN, 0, 3, 12)
    assert MessageHeader.decode(header.encode()) == header
    with pytest.raises(ProtocolError):
        MessageHeader.decode(header.encode()[:-1])


def test_ready_is_exact() -> None:
    ready = MessageHeader(MAGIC, VERSION, 0x80, 0, 0, 0)
    for invalid in (replace(ready, request_id=1), replace(ready, payload_length=1)):
        with pytest.raises(ProtocolError):
            decode_ready(invalid)
