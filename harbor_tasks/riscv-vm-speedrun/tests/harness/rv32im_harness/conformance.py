"""Run self-checking ELFs through one-shot ``run`` and persistent ``serve``."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
from dataclasses import dataclass
from pathlib import Path

from .vm_client import VmServer, run_once
from .vm_interface import RunOutcome

DEFAULT_MANIFEST = Path("conformance/artifacts/manifest.json")


class ConformanceFailure(RuntimeError):
    """A conformance input or VM result was invalid."""


@dataclass(frozen=True)
class _Case:
    name: str
    elf: bytes


def _load_cases(manifest: Path) -> list[_Case]:
    manifest = manifest.resolve()
    repository = manifest.parent.parent.parent
    try:
        document = json.loads(manifest.read_text())
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ConformanceFailure(f"cannot read manifest {manifest}: {error}") from error
    if not isinstance(document, dict):
        raise ConformanceFailure("manifest schema is invalid")
    records = document.get("cases")
    if document.get("schema_version") != 1 or not isinstance(records, list):
        raise ConformanceFailure("manifest schema is invalid")

    cases = []
    for record in records:
        try:
            suite = record["suite"]
            identifier = record["id"]
            elf_name = record["elf"]
            expected_hash = record["elf_sha256"]
        except (KeyError, TypeError) as error:
            raise ConformanceFailure("manifest case is invalid") from error
        if not all(
            isinstance(value, str)
            for value in (suite, identifier, elf_name, expected_hash)
        ):
            raise ConformanceFailure("manifest case is invalid")

        elf_path = (repository / elf_name).resolve()
        if not elf_path.is_relative_to(repository):
            raise ConformanceFailure(f"ELF path leaves the repository: {elf_name}")
        try:
            elf = elf_path.read_bytes()
        except OSError as error:
            raise ConformanceFailure(f"cannot read ELF {elf_name}: {error}") from error
        if hashlib.sha256(elf).hexdigest() != expected_hash:
            raise ConformanceFailure(
                f"ELF hash does not match the manifest: {elf_name}"
            )
        cases.append(_Case(f"{suite}/{identifier}", elf))

    if not cases:
        raise ConformanceFailure("manifest contains no cases")
    return cases


def _require_success(interface: str, case_name: str, outcome: RunOutcome) -> None:
    result = outcome.result
    if result.status != "exit":
        raise ConformanceFailure(
            f"{interface} {case_name}: expected exit, got {result.status}"
        )
    if result.exit_code != 0:
        raise ConformanceFailure(
            f"{interface} {case_name}: expected exit code 0, got {result.exit_code}"
        )
    if outcome.output:
        raise ConformanceFailure(
            f"{interface} {case_name}: expected empty output, got "
            f"{len(outcome.output)} bytes"
        )


def run_conformance(
    executable: str | os.PathLike[str],
    manifest: str | os.PathLike[str] = DEFAULT_MANIFEST,
) -> int:
    """Run every manifest case through one-shot and persistent VM interfaces."""

    cases = _load_cases(Path(manifest))

    for case in cases:
        try:
            outcome = run_once(executable, case.elf)
        except Exception as error:
            raise ConformanceFailure(f"one-shot {case.name}: {error}") from error
        _require_success("one-shot", case.name, outcome)

    try:
        with VmServer(executable) as server:
            for case in cases:
                try:
                    server.load(case.elf)
                    outcome = server.run()
                    server.unload()
                except Exception as error:
                    raise ConformanceFailure(f"serve {case.name}: {error}") from error
                _require_success("serve", case.name, outcome)
    except ConformanceFailure:
        raise
    except Exception as error:
        raise ConformanceFailure(f"serve: {error}") from error

    return len(cases)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Run self-checking RV32IM conformance ELFs."
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
        case_count = run_conformance(arguments.vm, arguments.manifest)
    except ConformanceFailure as error:
        print(f"conformance failed: {error}", file=sys.stderr)
        return 1
    print(
        f"conformance passed: {case_count} ELFs, "
        f"{case_count * 2} runs through both interfaces"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
