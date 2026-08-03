"""Procedural checks for invalid CLI arguments and protocol frames."""

from __future__ import annotations

import os
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Self

from .vm_client import (
    _MAX_RESULT_SIZE,
    _MAX_STATE_SIZE,
    _read_regular_file,
    _run_command,
    _ServerTransport,
)
from .vm_interface import (
    DEFAULT_INSTRUCTION_LIMIT,
    DEFAULT_OUTPUT_LIMIT,
    ERROR_MESSAGES,
    MAGIC,
    MAX_ELF_SIZE,
    MAX_INPUT_SIZE,
    MAX_INSTRUCTION_LIMIT,
    MAX_OUTPUT_LIMIT,
    MAX_PAYLOAD_SIZE,
    RUN_REQUEST_LAYOUT,
    VERSION,
    MessageHeader,
    Opcode,
    RunResult,
    Status,
    VMState,
    decode_ready,
    decode_run_response,
    encode_request,
    encode_run_request,
)


class InterfaceFailure(RuntimeError):
    """A procedural VM-interface check failed."""


@dataclass
class _Checks:
    completed: dict[str, str] = field(default_factory=dict)

    def done(self, *cases: tuple[str, str]) -> None:
        for case_id, spec in cases:
            if case_id in self.completed:
                raise RuntimeError(f"duplicate procedural case ID: {case_id}")
            self.completed[case_id] = spec


@dataclass(frozen=True)
class _CliResult:
    returncode: int
    stdout: bytes
    output: bytes | None
    result: bytes | None
    state: bytes | None


def _regular_bytes(path: Path, maximum: int, description: str) -> bytes | None:
    if not path.exists() or not path.is_file():
        return None
    return _read_regular_file(path, maximum, description)


def _invoke(
    executable: str | os.PathLike[str],
    elf: bytes,
    *,
    input_data: bytes = b"",
    omit: frozenset[str] = frozenset(),
    replace: dict[str, str] | None = None,
    extra: tuple[str, ...] = (),
    directories: frozenset[str] = frozenset(),
) -> _CliResult:
    with tempfile.TemporaryDirectory(prefix="rv32im-interface-") as temporary:
        work = Path(temporary)
        (work / "program.elf").write_bytes(elf)
        (work / "input.bin").write_bytes(input_data)
        for name in ("output.bin", "result.json", "state.json"):
            path = work / name
            if name in directories:
                path.mkdir()
            else:
                path.write_bytes(b"stale")

        values = {
            "--elf": "program.elf",
            "--input": "input.bin",
            "--output": "output.bin",
            "--result": "result.json",
        }
        values.update(replace or {})
        arguments = ["run"]
        for option, value in values.items():
            if option not in omit:
                arguments.extend((option, value))
        arguments.extend(extra)
        completed = _run_command(executable, arguments, cwd=work)
        return _CliResult(
            completed.returncode,
            completed.stdout,
            _regular_bytes(
                work / values["--output"],
                MAX_OUTPUT_LIMIT,
                "contract output",
            ),
            _regular_bytes(
                work / values["--result"],
                _MAX_RESULT_SIZE,
                "contract result",
            ),
            _regular_bytes(
                work / "state.json",
                _MAX_STATE_SIZE,
                "contract state",
            ),
        )


def _require_cli_success(result: _CliResult, name: str) -> RunResult:
    if result.returncode != 0:
        raise InterfaceFailure(f"{name}: expected process status zero")
    if result.stdout:
        raise InterfaceFailure(f"{name}: command wrote to standard output")
    if result.output is None or result.result is None:
        raise InterfaceFailure(f"{name}: output or result file is missing")
    parsed = RunResult.from_json_bytes(result.result)
    if parsed.output_length != len(result.output):
        raise InterfaceFailure(f"{name}: output length differs from result")
    return parsed


def _require_cli_exit(
    result: _CliResult,
    name: str,
    *,
    exit_code: int,
    retired: int,
    output: bytes = b"",
) -> None:
    parsed = _require_cli_success(result, name)
    if (
        parsed.status != "exit"
        or parsed.exit_code != exit_code
        or parsed.retired_instructions != retired
        or result.output != output
    ):
        raise InterfaceFailure(f"{name}: completed run differs")


def _require_cli_error(result: _CliResult, name: str) -> None:
    if result.returncode == 0:
        raise InterfaceFailure(f"{name}: expected nonzero process status")
    if result.stdout:
        raise InterfaceFailure(f"{name}: host error wrote to standard output")


def _require_command_error(
    executable: str | os.PathLike[str],
    arguments: tuple[str, ...],
    name: str,
) -> None:
    with tempfile.TemporaryDirectory(prefix="rv32im-interface-command-") as temporary:
        completed = _run_command(executable, arguments, cwd=temporary)
    if completed.returncode == 0:
        raise InterfaceFailure(f"{name}: expected nonzero process status")
    if completed.stdout:
        raise InterfaceFailure(f"{name}: host error wrote to standard output")


def _run_source_alias_contracts(
    executable: str | os.PathLike[str],
    write_elf: bytes,
) -> None:
    with tempfile.TemporaryDirectory(prefix="rv32im-interface-alias-") as temporary:
        work = Path(temporary)
        program = work / "program.elf"
        input_path = work / "input.bin"
        result_path = work / "result.json"
        original_input = b"source"
        program.write_bytes(write_elf)
        input_path.write_bytes(original_input)
        completed = _run_command(
            executable,
            (
                "run",
                "--elf",
                program.name,
                "--input",
                input_path.name,
                "--output",
                input_path.name,
                "--result",
                result_path.name,
            ),
            cwd=work,
        )
        if completed.returncode != 0 or completed.stdout:
            raise InterfaceFailure("output could not replace the input source")
        output = _read_regular_file(input_path, MAX_OUTPUT_LIMIT, "aliased output")
        result = RunResult.from_json_bytes(
            _read_regular_file(result_path, _MAX_RESULT_SIZE, "alias result")
        )
        if (
            output != original_input
            or result.status != "exit"
            or result.exit_code != 7
            or result.retired_instructions != 6
            or result.output_length != len(original_input)
        ):
            raise InterfaceFailure("output could not replace the input source")

        program.write_bytes(write_elf)
        input_path.write_bytes(b"")
        completed = _run_command(
            executable,
            (
                "run",
                "--elf",
                program.name,
                "--input",
                input_path.name,
                "--output",
                "output.bin",
                "--result",
                program.name,
            ),
            cwd=work,
        )
        if completed.returncode != 0 or completed.stdout:
            raise InterfaceFailure("result could not replace the ELF source")
        aliased_result = RunResult.from_json_bytes(
            _read_regular_file(program, _MAX_RESULT_SIZE, "aliased result")
        )
        aliased_output = _read_regular_file(
            work / "output.bin",
            MAX_OUTPUT_LIMIT,
            "alias output",
        )
        if (
            aliased_result.status != "exit"
            or aliased_result.exit_code != 7
            or aliased_result.retired_instructions != 6
            or aliased_result.output_length != 0
            or aliased_output
        ):
            raise InterfaceFailure("result could not replace the ELF source correctly")


def _run_cli_contracts(
    executable: str | os.PathLike[str],
    assets: dict[str, bytes],
    checks: _Checks,
) -> None:
    exit_elf = assets["image-a"]

    _require_command_error(executable, (), "missing command")
    _require_command_error(executable, ("unknown",), "unknown command")
    _require_command_error(executable, ("serve", "extra"), "serve argument")
    checks.done(
        ("cli-missing-command", "rv32vm-interface.md §1"),
        ("cli-unknown-command", "rv32vm-interface.md §1"),
        ("cli-serve-extra-argument", "rv32vm-interface.md §3"),
    )

    for option in ("--elf", "--input", "--output", "--result"):
        _require_cli_error(
            _invoke(executable, exit_elf, omit=frozenset({option})),
            f"missing {option}",
        )
    checks.done(
        *(
            (f"cli-missing-{option.removeprefix('--')}", "rv32vm-interface.md §1")
            for option in ("--elf", "--input", "--output", "--result")
        )
    )

    _require_cli_error(
        _invoke(executable, exit_elf, extra=("--unknown-option", "x")),
        "unknown option",
    )
    checks.done(("cli-unknown-option", "rv32vm-interface.md §1"))
    invalid_limits = (
        ("instruction-negative", "--instruction-limit", "-1"),
        ("instruction-signed", "--instruction-limit", "+1"),
        (
            "instruction-too-large",
            "--instruction-limit",
            str(MAX_INSTRUCTION_LIMIT + 1),
        ),
        ("output-negative", "--output-limit", "-1"),
        ("output-noninteger", "--output-limit", "1.0"),
        ("output-too-large", "--output-limit", "1048577"),
    )
    for case_id, option, invalid in invalid_limits:
        _require_cli_error(
            _invoke(executable, exit_elf, extra=(option, invalid)),
            f"invalid {option} {invalid}",
        )
        checks.done((f"cli-limit-{case_id}", "rv32vm-interface.md §1"))

    _require_cli_exit(
        _invoke(
            executable,
            exit_elf + bytes(MAX_ELF_SIZE - len(exit_elf)),
        ),
        "maximum ELF size",
        exit_code=11,
        retired=3,
    )
    _require_cli_error(
        _invoke(
            executable,
            exit_elf + bytes(MAX_ELF_SIZE + 1 - len(exit_elf)),
        ),
        "oversized ELF",
    )
    _require_cli_exit(
        _invoke(executable, exit_elf, input_data=bytes(MAX_INPUT_SIZE)),
        "maximum input size",
        exit_code=11,
        retired=3,
    )
    _require_cli_error(
        _invoke(executable, exit_elf, input_data=bytes(MAX_INPUT_SIZE + 1)),
        "oversized input",
    )
    checks.done(
        ("cli-elf-maximum", "rv32vm-interface.md §1"),
        ("cli-elf-too-large", "rv32vm-interface.md §1"),
        ("cli-input-maximum", "rv32vm-interface.md §1"),
        ("cli-input-too-large", "rv32vm-interface.md §1"),
    )
    maximum_output = _invoke(
        executable,
        assets["write-and-exit"],
        input_data=bytes(MAX_OUTPUT_LIMIT),
    )
    _require_cli_exit(
        maximum_output,
        "maximum output size",
        exit_code=7,
        retired=6,
        output=bytes(MAX_OUTPUT_LIMIT),
    )
    default_overflow = _invoke(
        executable,
        assets["write-and-exit"],
        input_data=bytes(MAX_OUTPUT_LIMIT + 1),
    )
    overflow_result = _require_cli_success(default_overflow, "default output limit")
    trap = overflow_result.trap
    if (
        overflow_result.status != "trap"
        or trap is None
        or trap.cause != "OutputLimitExceeded"
        or trap.value != MAX_OUTPUT_LIMIT + 1
        or overflow_result.retired_instructions != 1
        or default_overflow.output
    ):
        raise InterfaceFailure("default output limit is not exactly 1 MiB")
    checks.done(
        ("cli-output-maximum", "rv32vm-interface.md §1"),
        ("cli-output-default-boundary", "rv32vm-interface.md §1"),
    )

    default = _invoke(executable, exit_elf)
    explicit = _invoke(
        executable,
        exit_elf,
        extra=(
            "--instruction-limit",
            str(DEFAULT_INSTRUCTION_LIMIT),
            "--output-limit",
            str(DEFAULT_OUTPUT_LIMIT),
        ),
    )
    _require_cli_exit(default, "default limits", exit_code=11, retired=3)
    _require_cli_exit(
        explicit,
        "explicit default limits",
        exit_code=11,
        retired=3,
    )
    if (default.output, default.result) != (explicit.output, explicit.result):
        raise InterfaceFailure("default limits differ from explicit defaults")
    checks.done(("cli-default-limits", "rv32vm-interface.md §1"))

    replaced = _invoke(executable, exit_elf)
    _require_cli_exit(
        replaced,
        "destination replacement",
        exit_code=11,
        retired=3,
    )
    if replaced.output == b"stale" or replaced.result == b"stale":
        raise InterfaceFailure("destination files were not replaced")
    for option in ("--elf", "--input"):
        _require_cli_error(
            _invoke(
                executable,
                exit_elf,
                replace={option: "missing.bin"},
            ),
            f"inaccessible {option} source",
        )
        checks.done(
            (
                f"cli-inaccessible-{option.removeprefix('--')}",
                "rv32vm-interface.md §1",
            )
        )
    for destination in ("output.bin", "result.json"):
        _require_cli_error(
            _invoke(
                executable,
                exit_elf,
                directories=frozenset({destination}),
            ),
            f"unwritable {destination}",
        )
        checks.done(
            (
                f"cli-unwritable-{destination.split('.')[0]}",
                "rv32vm-interface.md §1",
            )
        )
    _require_cli_error(
        _invoke(
            executable,
            exit_elf,
            extra=("--state", "state.json"),
            directories=frozenset({"state.json"}),
        ),
        "unwritable state",
    )
    checks.done(("cli-unwritable-state", "rv32vm-interface.md §1.1"))
    _run_source_alias_contracts(executable, assets["write-and-exit"])
    checks.done(
        ("cli-replace-destinations", "rv32vm-interface.md §1"),
        ("cli-output-aliases-input", "rv32vm-interface.md §1"),
        ("cli-result-aliases-elf", "rv32vm-interface.md §1"),
    )

    invalid_inspections = (
        ("leading-zero", "00:0"),
        ("sign", "+1:0"),
        ("uppercase-prefix", "0X1:0"),
        ("whitespace", " 1:0"),
        ("missing-hex-digits", "0x:0"),
        ("missing-separator", "1"),
    )
    for case_id, text in invalid_inspections:
        _require_cli_error(
            _invoke(
                executable,
                exit_elf,
                extra=("--state", "state.json", "--inspect", text),
            ),
            f"invalid inspection {text!r}",
        )
        checks.done((f"cli-inspect-{case_id}", "rv32vm-interface.md §1.1"))
    _require_cli_error(
        _invoke(executable, exit_elf, extra=("--inspect", "0:0")),
        "inspection without state",
    )
    _require_cli_error(
        _invoke(
            executable,
            exit_elf,
            extra=("--state", "state.json", "--inspect", "0:1"),
        ),
        "inspection of unmapped memory",
    )
    checks.done(
        ("cli-inspect-without-state", "rv32vm-interface.md §1.1"),
        ("cli-inspect-unmapped", "rv32vm-interface.md §1.1"),
    )
    accepted_inspections = _invoke(
        executable,
        exit_elf,
        extra=(
            "--state",
            "state.json",
            "--inspect",
            "65536:4",
            "--inspect",
            "0x4000000:0",
        ),
    )
    _require_cli_exit(
        accepted_inspections,
        "valid inspections",
        exit_code=11,
        retired=3,
    )
    if accepted_inspections.state is None:
        raise InterfaceFailure("valid inspections did not produce state")
    state = VMState.from_json_bytes(accepted_inspections.state)
    if tuple((item.address, len(item.data)) for item in state.memory) != (
        (0x10000, 4),
        (0x4000000, 0),
    ):
        raise InterfaceFailure("inspection order or ranges differ")
    checks.done(("cli-inspect-valid-ranges", "rv32vm-interface.md §1.1"))

    zero_ranges = ("--state", "state.json") + (("--inspect", "0:0") * 1024)
    _require_cli_exit(
        _invoke(executable, exit_elf, extra=zero_ranges),
        "maximum inspection count",
        exit_code=11,
        retired=3,
    )
    _require_cli_error(
        _invoke(
            executable,
            exit_elf,
            extra=zero_ranges + ("--inspect", "0:0"),
        ),
        "oversized inspection count",
    )
    _require_cli_exit(
        _invoke(
            executable,
            exit_elf,
            extra=(
                "--state",
                "state.json",
                "--inspect",
                "0x3800000:0x800000",
            ),
        ),
        "maximum inspection bytes",
        exit_code=11,
        retired=3,
    )
    _require_cli_error(
        _invoke(
            executable,
            exit_elf,
            extra=(
                "--state",
                "state.json",
                "--inspect",
                "0x3800000:0x800000",
                "--inspect",
                "0x3800000:1",
            ),
        ),
        "oversized inspection bytes",
    )
    checks.done(
        ("cli-inspect-count-maximum", "rv32vm-interface.md §1.1"),
        ("cli-inspect-count-too-large", "rv32vm-interface.md §1.1"),
        ("cli-inspect-bytes-maximum", "rv32vm-interface.md §1.1"),
        ("cli-inspect-bytes-too-large", "rv32vm-interface.md §1.1"),
    )


class _RawServer:
    def __init__(self, executable: str | os.PathLike[str]) -> None:
        self.transport = _ServerTransport(executable)
        try:
            header, payload = self.transport.read_frame()
            decode_ready(header, payload)
        except BaseException:
            self.transport.close()
            raise

    def write(self, data: bytes) -> None:
        self.transport.write(data)

    def response(
        self,
        opcode: int,
        request_id: int,
        status: Status,
    ) -> bytes:
        header, payload = self.transport.read_frame()
        expected_opcode = opcode | 0x80
        if (
            header.magic != MAGIC
            or header.version != VERSION
            or header.opcode != expected_opcode
            or header.status_or_flags != status
            or header.request_id != request_id
            or header.payload_length != len(payload)
        ):
            raise InterfaceFailure(
                f"invalid response header for request {request_id}: {header!r}"
            )
        if status != Status.OK and payload != ERROR_MESSAGES[status]:
            raise InterfaceFailure(
                f"invalid {status.name} payload for request {request_id}"
            )
        return payload

    def request(
        self,
        opcode: Opcode,
        request_id: int,
        payload: bytes = b"",
    ) -> bytes:
        self.write(encode_request(opcode, request_id, payload))
        response = self.response(opcode, request_id, Status.OK)
        if opcode != Opcode.RUN and response:
            raise InterfaceFailure(f"{opcode.name} success payload is not empty")
        return response

    def shutdown(self, request_id: int = 0) -> None:
        self.request(Opcode.SHUTDOWN, request_id)
        returncode = self.transport.wait()
        if returncode != 0:
            raise InterfaceFailure(f"SHUTDOWN exited with status {returncode}")
        self.transport.require_eof()

    def close(self) -> None:
        self.transport.close()

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *_exception: object) -> None:
        self.close()


def _frame(
    opcode: int,
    request_id: int,
    payload: bytes = b"",
    *,
    magic: bytes = MAGIC,
    version: int = VERSION,
    flags: int = 0,
    payload_length: int | None = None,
) -> bytes:
    length = len(payload) if payload_length is None else payload_length
    return (
        MessageHeader(magic, version, opcode, flags, request_id, length).encode()
        + payload
    )


def _require_run_response(
    payload: bytes,
    status: str,
    *,
    exit_code: int | None = None,
    resource_failure: str | None = None,
    retired: int,
    output: bytes = b"",
) -> None:
    outcome = decode_run_response(payload)
    result = outcome.result
    if (
        result.status != status
        or result.exit_code != exit_code
        or (None if result.resource_failure is None else result.resource_failure.cause)
        != resource_failure
        or result.retired_instructions != retired
        or outcome.output != output
    ):
        raise InterfaceFailure(f"unexpected RUN result: {outcome!r}")


def _run_recoverable_protocol_contracts(
    executable: str | os.PathLike[str],
    exit_elf: bytes,
    invalid_elf: bytes,
    checks: _Checks,
) -> None:
    with _RawServer(executable) as server:
        requests = (
            _frame(Opcode.RESET, 0, version=2, flags=1)
            + _frame(0x7F, 0xFFFF_FFFF, flags=1)
            + _frame(0x7F, 1, b"x")
            + _frame(Opcode.LOAD, 2)
            + _frame(Opcode.RUN, 3, b"x")
            + _frame(Opcode.RESET, 4)
            + _frame(Opcode.UNLOAD, 5)
            + _frame(Opcode.RUN, 6, encode_run_request(0, 0, b""))
        )
        server.write(requests)
        expected = (
            (
                "protocol-version-before-flags",
                Opcode.RESET,
                0,
                Status.UNSUPPORTED_VERSION,
            ),
            (
                "protocol-flags-before-opcode",
                0x7F,
                0xFFFF_FFFF,
                Status.INVALID_FLAGS,
            ),
            ("protocol-opcode-before-payload", 0x7F, 1, Status.UNKNOWN_OPCODE),
            ("protocol-load-empty", Opcode.LOAD, 2, Status.INVALID_PAYLOAD),
            ("protocol-run-short", Opcode.RUN, 3, Status.INVALID_PAYLOAD),
            ("protocol-reset-empty-state", Opcode.RESET, 4, Status.INVALID_STATE),
            ("protocol-unload-empty-state", Opcode.UNLOAD, 5, Status.INVALID_STATE),
            ("protocol-run-empty-state", Opcode.RUN, 6, Status.INVALID_STATE),
        )
        for case_id, opcode, request_id, status in expected:
            server.response(opcode, request_id, status)
            checks.done((case_id, "rv32vm-interface.md §3.3"))

        server.request(Opcode.LOAD, 7, exit_elf)
        checks.done(("protocol-load", "rv32vm-interface.md §3.2"))
        server.write(
            _frame(Opcode.LOAD, 8, invalid_elf)
            + _frame(Opcode.RESET, 9, b"x")
            + _frame(
                Opcode.RUN,
                30,
                RUN_REQUEST_LAYOUT.pack(
                    MAX_INSTRUCTION_LIMIT,
                    MAX_OUTPUT_LIMIT,
                    1,
                ),
            )
            + _frame(
                Opcode.RUN,
                31,
                RUN_REQUEST_LAYOUT.pack(
                    MAX_INSTRUCTION_LIMIT,
                    MAX_OUTPUT_LIMIT,
                    0,
                )
                + b"x",
            )
            + _frame(
                Opcode.RUN,
                10,
                encode_run_request(MAX_INSTRUCTION_LIMIT, MAX_OUTPUT_LIMIT, b""),
            )
            + _frame(
                Opcode.RUN,
                11,
                RUN_REQUEST_LAYOUT.pack(MAX_INSTRUCTION_LIMIT + 1, 0, 0),
            )
            + _frame(
                Opcode.RUN,
                12,
                RUN_REQUEST_LAYOUT.pack(0, MAX_OUTPUT_LIMIT + 1, 0),
            )
        )
        server.response(Opcode.LOAD, 8, Status.INVALID_STATE)
        server.response(Opcode.RESET, 9, Status.INVALID_PAYLOAD)
        server.response(Opcode.RUN, 30, Status.INVALID_PAYLOAD)
        server.response(Opcode.RUN, 31, Status.INVALID_PAYLOAD)
        _require_run_response(
            server.response(Opcode.RUN, 10, Status.OK),
            "exit",
            exit_code=11,
            retired=3,
        )
        server.response(Opcode.RUN, 11, Status.INVALID_PAYLOAD)
        server.response(Opcode.RUN, 12, Status.INVALID_PAYLOAD)
        checks.done(
            ("protocol-load-state-before-elf", "rv32vm-interface.md §3.3"),
            ("protocol-reset-payload-before-state", "rv32vm-interface.md §3.3"),
            ("protocol-run-input-short", "rv32vm-interface.md §3.2"),
            ("protocol-run-input-trailing", "rv32vm-interface.md §3.2"),
            ("protocol-run-limit-maximum", "rv32vm-interface.md §3.2"),
            ("protocol-instruction-limit-too-large", "rv32vm-interface.md §3.2"),
            ("protocol-output-limit-too-large", "rv32vm-interface.md §3.2"),
        )

        maximum_input = bytes(MAX_INPUT_SIZE)
        _require_run_response(
            server.request(
                Opcode.RUN,
                13,
                encode_run_request(0, 0, maximum_input),
            ),
            "resource_failure",
            resource_failure="InstructionLimit",
            retired=0,
        )
        checks.done(("protocol-input-maximum", "rv32vm-interface.md §3.2"))
        oversized_input_header = (
            (0).to_bytes(8, "little")
            + (0).to_bytes(4, "little")
            + (MAX_INPUT_SIZE + 1).to_bytes(4, "little")
        )
        server.write(
            _frame(
                Opcode.RUN,
                14,
                oversized_input_header + bytes(MAX_INPUT_SIZE + 1),
            )
        )
        server.response(Opcode.RUN, 14, Status.INVALID_PAYLOAD)
        checks.done(("protocol-input-too-large", "rv32vm-interface.md §3.2"))

        server.request(Opcode.RESET, 15)
        server.write(_frame(Opcode.UNLOAD, 16, b"x"))
        server.response(Opcode.UNLOAD, 16, Status.INVALID_PAYLOAD)
        server.request(Opcode.UNLOAD, 17)
        server.write(_frame(Opcode.RUN, 18, encode_run_request(0, 0, b"")))
        server.response(Opcode.RUN, 18, Status.INVALID_STATE)
        server.write(_frame(Opcode.SHUTDOWN, 19, b"x"))
        server.response(Opcode.SHUTDOWN, 19, Status.INVALID_PAYLOAD)
        server.shutdown(20)
        checks.done(
            ("protocol-reset", "rv32vm-interface.md §3.2"),
            ("protocol-unload-nonempty", "rv32vm-interface.md §3.2"),
            ("protocol-unload", "rv32vm-interface.md §3.2"),
            ("protocol-run-after-unload", "rv32vm-interface.md §3.4"),
            ("protocol-shutdown-nonempty", "rv32vm-interface.md §3.2"),
            ("protocol-shutdown", "rv32vm-interface.md §3.2"),
        )


def _run_payload_boundary_contract(
    executable: str | os.PathLike[str],
    exit_elf: bytes,
    checks: _Checks,
) -> None:
    padded = exit_elf + bytes(MAX_PAYLOAD_SIZE - len(exit_elf))
    with _RawServer(executable) as server:
        server.request(Opcode.LOAD, 1, padded)
        server.request(Opcode.UNLOAD, 2)
        server.shutdown(3)
    checks.done(("protocol-payload-maximum", "rv32vm-interface.md §3.1"))


def _run_output_boundary_contract(
    executable: str | os.PathLike[str],
    write_elf: bytes,
    checks: _Checks,
) -> None:
    input_data = bytes(MAX_OUTPUT_LIMIT)
    with _RawServer(executable) as server:
        server.request(Opcode.LOAD, 1, write_elf)
        payload = server.request(
            Opcode.RUN,
            2,
            encode_run_request(
                MAX_INSTRUCTION_LIMIT,
                MAX_OUTPUT_LIMIT,
                input_data,
            ),
        )
        outcome = decode_run_response(payload)
        if (
            outcome.result.status != "exit"
            or outcome.result.exit_code != 7
            or outcome.result.retired_instructions != 6
            or outcome.output != input_data
        ):
            raise InterfaceFailure("serve maximum output was not returned in full")
        server.shutdown(3)
    checks.done(("protocol-output-maximum", "rv32vm-interface.md §3.2"))


def _run_terminal_contract(
    executable: str | os.PathLike[str],
    frame: bytes,
    *,
    opcode: int | None = None,
    request_id: int | None = None,
    status: Status | None = None,
) -> None:
    with _RawServer(executable) as server:
        server.write(frame)
        server.transport.close_input()
        if status is not None:
            assert opcode is not None and request_id is not None
            server.response(opcode, request_id, status)
        returncode = server.transport.wait()
        if returncode == 0:
            raise InterfaceFailure("terminal framing error exited with status zero")
        server.transport.require_eof()


def _run_terminal_protocol_contracts(
    executable: str | os.PathLike[str],
    checks: _Checks,
) -> None:
    _run_terminal_contract(
        executable,
        _frame(Opcode.RESET, 21, magic=b"NOPE"),
        opcode=Opcode.RESET,
        request_id=21,
        status=Status.MALFORMED_FRAME,
    )
    checks.done(("protocol-bad-magic", "rv32vm-interface.md §3.3"))
    _run_terminal_contract(
        executable,
        _frame(
            Opcode.LOAD,
            22,
            payload_length=MAX_PAYLOAD_SIZE + 1,
        ),
        opcode=Opcode.LOAD,
        request_id=22,
        status=Status.FRAME_TOO_LARGE,
    )
    checks.done(("protocol-frame-too-large", "rv32vm-interface.md §3.3"))
    _run_terminal_contract(
        executable,
        _frame(Opcode.LOAD, 23, payload_length=1),
        opcode=Opcode.LOAD,
        request_id=23,
        status=Status.MALFORMED_FRAME,
    )
    checks.done(("protocol-truncated-payload", "rv32vm-interface.md §3.3"))
    correlated = _frame(Opcode.RESET, 24)[:12]
    _run_terminal_contract(
        executable,
        correlated,
        opcode=Opcode.RESET,
        request_id=24,
        status=Status.MALFORMED_FRAME,
    )
    checks.done(("protocol-truncated-header-correlated", "rv32vm-interface.md §3.3"))
    _run_terminal_contract(executable, b"")
    checks.done(("protocol-eof-before-shutdown", "rv32vm-interface.md §3.2"))


def run_interface_contracts(
    executable: str | os.PathLike[str],
    assets: dict[str, bytes],
) -> int:
    """Run direct CLI and raw persistent-protocol checks."""

    checks = _Checks()
    _run_cli_contracts(executable, assets, checks)
    _run_recoverable_protocol_contracts(
        executable,
        assets["image-a"],
        assets["bad-elf-magic"],
        checks,
    )
    _run_payload_boundary_contract(executable, assets["image-a"], checks)
    _run_output_boundary_contract(executable, assets["write-and-exit"], checks)
    _run_terminal_protocol_contracts(executable, checks)
    return len(checks.completed)
