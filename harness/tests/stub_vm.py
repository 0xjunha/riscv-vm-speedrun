#!/usr/bin/env python3
"""Stub ``rv32vm`` process used only by the VM client tests."""

from __future__ import annotations

import argparse
import base64
import json
import os
import struct
import subprocess
import time
from pathlib import Path

MAGIC = b"RV32"
HEADER = struct.Struct("<4sBBHII")
RUN_REQUEST = struct.Struct("<QII")
RUN_RESPONSE = struct.Struct("<II")

LOAD, RUN, RESET, UNLOAD, SHUTDOWN = range(1, 6)
OK, INVALID_STATE = 0, 7
TERMINAL_MESSAGES = {
    1: b"malformed frame",
    5: b"frame too large",
    9: b"internal error",
}


def _json(value: object) -> bytes:
    return json.dumps(value, separators=(",", ":")).encode()


def _result(output: bytes, retired: int = 3) -> bytes:
    return _json(
        {
            "schema_version": 1,
            "status": "exit",
            "exit_code": 0,
            "trap": None,
            "resource_failure": None,
            "retired_instructions": retired,
            "output_length": len(output),
        }
    )


def _state(output: bytes, inspections: list[str], mode: str) -> bytes:
    memory = []
    for inspection in inspections:
        address_text, length_text = inspection.split(":")
        address, length = int(address_text, 0), int(length_text, 0)
        data = bytes((address + offset) & 0xFF for offset in range(length))
        memory.append(
            {
                "address": address,
                "data_base64": base64.b64encode(data).decode(),
            }
        )
    value = {
        "schema_version": 1,
        "pc": 0x10000,
        "registers": [0] * 32,
        "memory": memory,
        "retired_instructions": 3,
        "output_length": len(output),
    }
    if mode == "state-output-mismatch":
        value["output_length"] = len(output) + 1
    elif mode == "state-retirement-mismatch":
        value["retired_instructions"] = 4
    elif mode == "state-ranges-mismatch":
        value["memory"] = []
    return _json(value)


def _record_pids(*pids: int) -> None:
    if path := os.environ.get("STUB_VM_PID_FILE"):
        Path(path).write_text("\n".join(str(pid) for pid in pids))


def _run(arguments: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--elf", required=True)
    parser.add_argument("--input", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--result", required=True)
    parser.add_argument("--instruction-limit", required=True, type=int)
    parser.add_argument("--output-limit", required=True, type=int)
    parser.add_argument("--state")
    parser.add_argument("--inspect", action="append", default=[])
    options = parser.parse_args(arguments)
    mode = os.environ.get("STUB_VM_MODE", "")

    if mode == "hang-with-child":
        child = subprocess.Popen(
            [os.sys.executable, "-c", "import time; time.sleep(60)"]
        )
        _record_pids(os.getpid(), child.pid)
        time.sleep(60)
    if mode == "hang":
        _record_pids(os.getpid())
        time.sleep(60)
    if mode == "host-error":
        print("deliberate host failure", file=os.sys.stderr)
        return 7
    if mode == "flood-host-error":
        chunk = b"x" * (64 * 1024)
        for _ in range(32):
            os.write(1, chunk)
            os.write(2, chunk)
        return 7

    Path(options.elf).read_bytes()
    output = Path(options.input).read_bytes()
    output_path = Path(options.output)
    if mode == "oversized-output":
        output = bytes(options.output_limit + 1)
    if mode == "nonregular-output":
        output_path.unlink()
        output_path.mkdir()
    else:
        output_path.write_bytes(output)

    result_path = Path(options.result)
    if mode == "missing-result":
        result_path.unlink()
    elif mode == "malformed-result":
        result_path.write_bytes(b"{}")
    else:
        retired = options.instruction_limit + 1 if mode == "too-many-retired" else 3
        result_path.write_bytes(_result(output, retired))
    if options.state is not None:
        state_path = Path(options.state)
        if mode == "missing-state":
            state_path.unlink()
        elif mode == "nonregular-state":
            state_path.unlink()
            state_path.mkdir()
        elif mode == "oversized-state":
            state_path.write_bytes(bytes(12 * 1024 * 1024 + 1))
        else:
            state_path.write_bytes(_state(output, options.inspect, mode))
    return 0


def _read_exact(length: int) -> bytes:
    data = bytearray()
    while len(data) < length:
        chunk = os.read(0, length - len(data))
        if not chunk:
            break
        data.extend(chunk)
    return bytes(data)


def _respond(opcode: int, request_id: int, status: int, payload: bytes = b"") -> None:
    os.write(
        1,
        HEADER.pack(MAGIC, 1, opcode | 0x80, status, request_id, len(payload))
        + payload,
    )


def _log(name: str) -> None:
    if path := os.environ.get("STUB_VM_LOG"):
        with Path(path).open("a", encoding="utf-8") as stream:
            print(name, file=stream)


def _serve() -> int:
    mode = os.environ.get("STUB_VM_MODE", "")
    _record_pids(os.getpid())
    ready_magic = b"NOPE" if mode == "bad-ready" else MAGIC
    os.write(1, HEADER.pack(ready_magic, 1, 0x80, OK, 0, 0))
    if mode == "server-no-read":
        time.sleep(60)
    loaded = False
    while True:
        raw_header = _read_exact(HEADER.size)
        if len(raw_header) != HEADER.size:
            return 2
        _magic, _version, opcode, _flags, request_id, length = HEADER.unpack(raw_header)
        payload = _read_exact(length)
        if mode == "server-hang":
            time.sleep(60)
        response_id = request_id + 1 if mode == "bad-correlation" else request_id

        if opcode == LOAD:
            if mode == "server-stderr-flood":
                for _ in range(32):
                    os.write(2, b"x" * (64 * 1024))
            if mode.startswith("terminal-"):
                status = int(mode.removeprefix("terminal-"))
                _respond(opcode, response_id, status, TERMINAL_MESSAGES[status])
                continue
            loaded = True
            _log("load")
            response = b"x" if mode == "load-success-payload" else b""
            _respond(opcode, response_id, OK, response)
        elif opcode == RUN:
            if not loaded:
                _respond(opcode, response_id, INVALID_STATE, b"invalid state")
                continue
            if mode == "run-malformed-success":
                _respond(opcode, response_id, OK, b"x")
                continue
            instruction_limit, _output_limit, input_length = RUN_REQUEST.unpack_from(
                payload
            )
            output = payload[RUN_REQUEST.size :]
            assert len(output) == input_length
            retired = instruction_limit + 1 if mode == "too-many-retired" else 3
            result = _result(output, retired)
            _log("run")
            _respond(
                opcode,
                response_id,
                OK,
                RUN_RESPONSE.pack(len(result), len(output)) + result + output,
            )
        elif opcode == RESET:
            _log("reset")
            response = b"x" if mode == "reset-success-payload" else b""
            _respond(opcode, response_id, OK, response)
        elif opcode == UNLOAD:
            loaded = False
            _log("unload")
            response = b"x" if mode == "unload-success-payload" else b""
            _respond(opcode, response_id, OK, response)
        elif opcode == SHUTDOWN:
            _log("shutdown")
            response = b"x" if mode == "shutdown-success-payload" else b""
            _respond(opcode, response_id, OK, response)
            if mode == "shutdown-hang":
                time.sleep(60)
            if mode == "shutdown-nonzero":
                return 7
            if mode == "shutdown-trailing-output":
                os.write(1, b"x")
            return 0


def main() -> int:
    if len(os.sys.argv) < 2:
        return 2
    if os.sys.argv[1] == "run":
        return _run(os.sys.argv[2:])
    if os.sys.argv[1] == "serve":
        return _serve()
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
