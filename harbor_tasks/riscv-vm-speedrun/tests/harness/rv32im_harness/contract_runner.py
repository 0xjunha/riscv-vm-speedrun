"""Run the EEI and VM-interface contract suite against an ``rv32vm``."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .interface_contracts import run_interface_contracts
from .vm_client import (
    VmServer,
    _run_command,
    run_once,
)
from .vm_interface import (
    DEFAULT_INSTRUCTION_LIMIT,
    DEFAULT_OUTPUT_LIMIT,
    ProtocolResponseError,
    RunOutcome,
    Status,
)

DEFAULT_MANIFEST = Path("contracts/artifacts/manifest.json")


class ContractFailure(RuntimeError):
    """A contract input or VM result violated the suite's expectations."""


@dataclass(frozen=True)
class _Case:
    case_id: str
    kind: str
    elf: bytes
    record: dict[str, Any]


def _load_cases(manifest_path: Path) -> list[_Case]:
    manifest_path = manifest_path.resolve()
    repository = manifest_path.parent.parent.parent
    try:
        document = json.loads(manifest_path.read_text())
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ContractFailure(
            f"cannot read manifest {manifest_path}: {error}"
        ) from error
    records = document.get("cases") if isinstance(document, dict) else None
    if (
        not isinstance(document, dict)
        or document.get("schema_version") != 1
        or not isinstance(records, list)
    ):
        raise ContractFailure("manifest schema is invalid")

    cases = []
    seen = set()
    for record in records:
        if not isinstance(record, dict):
            raise ContractFailure("manifest contains a non-object case")
        case_id = record.get("id")
        kind = record.get("kind")
        elf_name = record.get("elf")
        expected_hash = record.get("elf_sha256")
        if (
            not isinstance(case_id, str)
            or not case_id
            or case_id in seen
            or kind not in {"execute", "reject"}
            or not isinstance(elf_name, str)
            or not isinstance(expected_hash, str)
            or not isinstance(record.get("symbols"), dict)
        ):
            raise ContractFailure(f"manifest case is invalid: {case_id!r}")
        seen.add(case_id)
        elf_path = (repository / elf_name).resolve()
        if not elf_path.is_relative_to(repository):
            raise ContractFailure(f"ELF path leaves the repository: {elf_name}")
        try:
            elf = elf_path.read_bytes()
        except OSError as error:
            raise ContractFailure(f"cannot read ELF {elf_name}: {error}") from error
        if hashlib.sha256(elf).hexdigest() != expected_hash:
            raise ContractFailure(f"ELF hash does not match manifest: {elf_name}")
        cases.append(_Case(case_id, kind, elf, record))
    if not cases:
        raise ContractFailure("manifest contains no cases")
    return cases


def _address(value: object, case: _Case) -> int:
    if isinstance(value, int) and not isinstance(value, bool):
        return value
    symbols = case.record["symbols"]
    if isinstance(value, str) and value in symbols:
        address = symbols[value]
        if isinstance(address, int) and not isinstance(address, bool):
            return address
    raise ContractFailure(f"{case.case_id}: unknown address {value!r}")


def _require_result(case: _Case, run: dict[str, Any], outcome: RunOutcome) -> None:
    expected = run["result"]
    result = outcome.result
    status = expected["status"]
    if result.status != status:
        raise ContractFailure(
            f"{case.case_id}: expected status {status}, got {result.status}"
        )
    if result.retired_instructions != expected["retired_instructions"]:
        raise ContractFailure(
            f"{case.case_id}: expected {expected['retired_instructions']} retired "
            f"instructions, got {result.retired_instructions}"
        )
    if status == "exit":
        if result.exit_code != expected["exit_code"]:
            raise ContractFailure(
                f"{case.case_id}: expected exit code {expected['exit_code']}, "
                f"got {result.exit_code}"
            )
    elif status == "trap":
        trap = result.trap
        trap_expected = expected["trap"]
        if trap is None or (
            trap.cause,
            trap.pc,
            trap.value,
        ) != (
            trap_expected["cause"],
            _address(trap_expected["pc"], case),
            trap_expected["value"],
        ):
            raise ContractFailure(f"{case.case_id}: unexpected trap {trap!r}")
    elif (
        result.resource_failure is None
        or result.resource_failure.cause != expected["resource_failure"]
    ):
        raise ContractFailure(
            f"{case.case_id}: unexpected resource failure {result.resource_failure!r}"
        )

    expected_output = bytes.fromhex(run.get("output_hex", ""))
    if outcome.output != expected_output:
        raise ContractFailure(
            f"{case.case_id}: expected {len(expected_output)} output bytes, got "
            f"{len(outcome.output)}"
        )


def _inspections(run: dict[str, Any]) -> tuple[tuple[int, int], ...]:
    state = run.get("state", {})
    return tuple(
        (item["address"], len(bytes.fromhex(item["data_hex"])))
        for item in state.get("memory", [])
    )


def _require_state(case: _Case, run: dict[str, Any], outcome: RunOutcome) -> None:
    expected = run.get("state")
    if expected is None:
        return
    state = outcome.state
    if state is None:
        raise ContractFailure(f"{case.case_id}: one-shot state is missing")
    if "pc" in expected and state.pc != _address(expected["pc"], case):
        raise ContractFailure(
            f"{case.case_id}: expected pc {_address(expected['pc'], case)}, "
            f"got {state.pc}"
        )
    registers = expected.get("registers")
    if isinstance(registers, list):
        if tuple(registers) != state.registers:
            raise ContractFailure(f"{case.case_id}: register state differs")
    elif isinstance(registers, dict):
        for index_text, value in registers.items():
            index = int(index_text)
            if state.registers[index] != value:
                raise ContractFailure(
                    f"{case.case_id}: expected x{index}={value}, "
                    f"got {state.registers[index]}"
                )
    memory = expected.get("memory", [])
    actual = tuple((item.address, item.data) for item in state.memory)
    wanted = tuple(
        (item["address"], bytes.fromhex(item["data_hex"])) for item in memory
    )
    if actual != wanted:
        raise ContractFailure(f"{case.case_id}: inspected memory differs")


def _run_arguments(run: dict[str, Any]) -> dict[str, Any]:
    return {
        "input_data": bytes.fromhex(run.get("input_hex", "")),
        "instruction_limit": run.get("instruction_limit", DEFAULT_INSTRUCTION_LIMIT),
        "output_limit": run.get("output_limit", DEFAULT_OUTPUT_LIMIT),
    }


def _run_executable_cases(
    executable: str | os.PathLike[str],
    cases: list[_Case],
) -> int:
    count = 0
    for case in cases:
        for run in case.record["runs"]:
            arguments = _run_arguments(run)
            for _ in range(run.get("repeat", 1)):
                try:
                    outcome = run_once(
                        executable,
                        case.elf,
                        **arguments,
                        capture_state="state" in run,
                        inspections=_inspections(run),
                    )
                except Exception as error:
                    raise ContractFailure(
                        f"one-shot {case.case_id}: {error}"
                    ) from error
                _require_result(case, run, outcome)
                _require_state(case, run, outcome)
                count += 1

    try:
        with VmServer(executable) as server:
            for case in cases:
                server.load(case.elf)
                for run in case.record["runs"]:
                    arguments = _run_arguments(run)
                    for _ in range(run.get("repeat", 1)):
                        outcome = server.run(**arguments)
                        _require_result(case, run, outcome)
                        count += 1
                server.unload()
    except ContractFailure:
        raise
    except Exception as error:
        raise ContractFailure(f"serve executable cases: {error}") from error
    return count


def _require_one_shot_rejection(
    executable: str | os.PathLike[str],
    case: _Case,
) -> None:
    with tempfile.TemporaryDirectory(prefix="rv32im-contract-reject-") as temporary:
        work = Path(temporary)
        (work / "program.elf").write_bytes(case.elf)
        (work / "input.bin").write_bytes(b"")
        completed = _run_command(
            executable,
            (
                "run",
                "--elf",
                "program.elf",
                "--input",
                "input.bin",
                "--output",
                "output.bin",
                "--result",
                "result.json",
            ),
            cwd=work,
        )
    if completed.returncode == 0:
        raise ContractFailure(f"one-shot {case.case_id}: invalid ELF was accepted")
    if completed.stdout:
        raise ContractFailure(
            f"one-shot {case.case_id}: host error wrote to standard output"
        )


def _run_rejected_cases(
    executable: str | os.PathLike[str],
    cases: list[_Case],
    valid_elf: bytes,
) -> int:
    for case in cases:
        _require_one_shot_rejection(executable, case)
    try:
        with VmServer(executable) as server:
            for case in cases:
                try:
                    server.load(case.elf)
                except ProtocolResponseError as error:
                    if error.status != Status.ELF_REJECTED:
                        raise ContractFailure(
                            f"serve {case.case_id}: expected ElfRejected, "
                            f"got {error.status.name}"
                        ) from error
                else:
                    raise ContractFailure(
                        f"serve {case.case_id}: invalid ELF was accepted"
                    )
            server.load(valid_elf)
            server.unload()
    except ContractFailure:
        raise
    except Exception as error:
        raise ContractFailure(f"serve rejected ELF cases: {error}") from error
    return len(cases) * 2


def _run_image_lifecycle(
    executable: str | os.PathLike[str],
    by_id: dict[str, _Case],
) -> int:
    expected = (("image-a", 11), ("image-b", 22), ("image-a", 11))
    try:
        with VmServer(executable) as server:
            for case_id, exit_code in expected:
                case = by_id[case_id]
                server.load(case.elf)
                outcome = server.run()
                _require_result(case, case.record["runs"][0], outcome)
                if outcome.result.exit_code != exit_code:
                    raise ContractFailure(
                        f"serve lifecycle {case_id}: expected exit {exit_code}"
                    )
                server.unload()
    except ContractFailure:
        raise
    except Exception as error:
        raise ContractFailure(f"serve image lifecycle: {error}") from error
    return len(expected)


def run_contracts(
    executable: str | os.PathLike[str],
    manifest: str | os.PathLike[str] = DEFAULT_MANIFEST,
) -> int:
    """Run all generated and procedural contract checks."""

    cases = _load_cases(Path(manifest))
    executable_cases = [case for case in cases if case.kind == "execute"]
    rejected_cases = [case for case in cases if case.kind == "reject"]
    by_id = {case.case_id: case for case in cases}

    count = _run_executable_cases(executable, executable_cases)
    count += _run_rejected_cases(executable, rejected_cases, by_id["image-a"].elf)
    count += _run_image_lifecycle(executable, by_id)
    try:
        count += run_interface_contracts(
            executable,
            {case_id: case.elf for case_id, case in by_id.items()},
        )
    except Exception as error:
        raise ContractFailure(f"VM interface: {error}") from error
    return count


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Run RV32IM EEI and VM-interface contract tests."
    )
    parser.add_argument("vm", help="path to an rv32vm executable")
    parser.add_argument(
        "manifest",
        nargs="?",
        default=DEFAULT_MANIFEST,
        help=f"manifest path (default: {DEFAULT_MANIFEST})",
    )
    arguments = parser.parse_args()
    try:
        count = run_contracts(arguments.vm, arguments.manifest)
    except ContractFailure as error:
        print(f"contracts failed: {error}", file=sys.stderr)
        return 1
    print(f"contracts passed: {count} checks")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
