"""Measure manifest-defined workloads in isolated ``rv32vm serve`` processes."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
import statistics
import sys
import time
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .vm_client import VmServer
from .vm_interface import (
    MAX_ELF_SIZE,
    MAX_INPUT_SIZE,
    MAX_INSTRUCTION_LIMIT,
    RunOutcome,
)

DEFAULT_MANIFEST = Path("benchmarks/artifacts/manifest.json")
DEFAULT_WARMUPS = 2
DEFAULT_REPETITIONS = 7
DEFAULT_TIMEOUT = 30.0
RESULT_SIZE = 8
MANIFEST_KEYS = frozenset({"schema_version", "application_workloads", "cases"})
MANIFEST_CASE_KEYS = frozenset(
    {
        "id",
        "workload",
        "elf",
        "elf_sha256",
        "input",
        "input_sha256",
        "expected_output_hex",
        "instruction_limit",
    }
)


class BenchmarkFailure(RuntimeError):
    """A benchmark asset, VM result, or runner configuration was invalid."""


@dataclass(frozen=True)
class BenchmarkCase:
    """One fully validated benchmark case loaded into immutable memory."""

    case_id: str
    workload: str
    elf: bytes
    input_data: bytes
    expected_output: bytes
    instruction_limit: int


@dataclass(frozen=True)
class BenchmarkSuite:
    """A validated manifest and its immutable in-memory artifact payloads."""

    sha256: str
    cases: tuple[BenchmarkCase, ...]
    application_workloads: tuple[str, ...]

    def select(self, case_ids: Sequence[str] | None = None) -> BenchmarkSuite:
        """Return a case selection without rereading or rehashing artifacts."""

        selected = _select_cases(self.cases, case_ids)
        if selected is self.cases:
            return self
        workloads = {case.workload for case in selected}
        application_workloads = tuple(
            workload for workload in self.application_workloads if workload in workloads
        )
        return BenchmarkSuite(self.sha256, selected, application_workloads)


def _bounded_integer(
    record: dict[str, Any],
    key: str,
    maximum: int,
    case_id: str,
) -> int:
    value = record.get(key)
    if type(value) is not int or not 0 <= value <= maximum:
        raise BenchmarkFailure(f"{case_id}: {key} is invalid")
    return value


def _read_artifact(
    root: Path,
    record: dict[str, Any],
    key: str,
    case_id: str,
    maximum_size: int,
) -> bytes:
    name = record.get(key)
    expected_hash = record.get(f"{key}_sha256")
    if (
        not isinstance(name, str)
        or not name
        or not isinstance(expected_hash, str)
        or len(expected_hash) != 64
        or any(character not in "0123456789abcdef" for character in expected_hash)
    ):
        raise BenchmarkFailure(f"{case_id}: {key} metadata is invalid")

    def require_file(metadata: os.stat_result) -> None:
        if not stat.S_ISREG(metadata.st_mode):
            raise BenchmarkFailure(f"{case_id}: {key} is not a regular file")
        if metadata.st_size > maximum_size:
            raise BenchmarkFailure(
                f"{case_id}: {key} size exceeds its {maximum_size}-byte limit"
            )

    try:
        relative = Path(name)
        path = (root / relative).resolve()
        if relative.is_absolute() or not path.is_relative_to(root):
            raise BenchmarkFailure(
                f"{case_id}: {key} path leaves the artifact directory"
            )
        initial = path.stat()
        require_file(initial)
        with path.open("rb") as stream:
            opened = os.fstat(stream.fileno())
            require_file(opened)
            if opened.st_size != initial.st_size:
                raise BenchmarkFailure(f"{case_id}: {key} changed while opening")
            data = stream.read(maximum_size + 1)
            final = os.fstat(stream.fileno())
    except BenchmarkFailure:
        raise
    except (OSError, RuntimeError, ValueError) as error:
        raise BenchmarkFailure(
            f"{case_id}: cannot read {key} {name}: {error}"
        ) from error
    if len(data) > maximum_size:
        raise BenchmarkFailure(
            f"{case_id}: {key} size exceeds its {maximum_size}-byte limit"
        )
    if final.st_size != opened.st_size or len(data) != opened.st_size:
        raise BenchmarkFailure(f"{case_id}: {key} changed while reading")
    if hashlib.sha256(data).hexdigest() != expected_hash:
        raise BenchmarkFailure(f"{case_id}: {key} hash does not match the manifest")
    return data


def _expected_output(record: dict[str, Any], case_id: str) -> bytes:
    value = record.get("expected_output_hex")
    if not isinstance(value, str) or len(value) != RESULT_SIZE * 2:
        raise BenchmarkFailure(f"{case_id}: expected_output_hex is invalid")
    try:
        output = bytes.fromhex(value)
    except ValueError as error:
        raise BenchmarkFailure(f"{case_id}: expected_output_hex is invalid") from error
    if len(output) != RESULT_SIZE:
        raise BenchmarkFailure(f"{case_id}: expected_output_hex is invalid")
    return output


def load_benchmark_suite(
    manifest: str | os.PathLike[str] = DEFAULT_MANIFEST,
) -> BenchmarkSuite:
    """Load, validate, and hash a benchmark suite and all referenced artifacts."""

    path = Path(manifest).resolve()
    try:
        payload = path.read_bytes()
        document = json.loads(payload)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise BenchmarkFailure(f"cannot read manifest {path}: {error}") from error
    if not isinstance(document, dict) or set(document) != MANIFEST_KEYS:
        raise BenchmarkFailure("manifest schema is invalid")
    records = document["cases"]
    application_workloads = document["application_workloads"]
    if (
        document["schema_version"] != 1
        or not isinstance(records, list)
        or not isinstance(application_workloads, list)
        or any(
            not isinstance(workload, str) or not workload
            for workload in application_workloads
        )
        or len(set(application_workloads)) != len(application_workloads)
    ):
        raise BenchmarkFailure("manifest schema is invalid")

    root = path.parent
    cases = []
    seen = set()
    for record in records:
        if not isinstance(record, dict) or set(record) != MANIFEST_CASE_KEYS:
            raise BenchmarkFailure("manifest case fields are invalid")
        case_id = record.get("id")
        workload = record.get("workload")
        if (
            not isinstance(case_id, str)
            or not case_id
            or case_id in seen
            or not isinstance(workload, str)
            or not workload
        ):
            raise BenchmarkFailure(f"manifest case is invalid: {case_id!r}")
        seen.add(case_id)
        instruction_limit = _bounded_integer(
            record, "instruction_limit", MAX_INSTRUCTION_LIMIT, case_id
        )
        elf = _read_artifact(root, record, "elf", case_id, MAX_ELF_SIZE)
        input_data = _read_artifact(root, record, "input", case_id, MAX_INPUT_SIZE)
        expected_output = _expected_output(record, case_id)
        cases.append(
            BenchmarkCase(
                case_id,
                workload,
                elf,
                input_data,
                expected_output,
                instruction_limit,
            )
        )

    if not cases:
        raise BenchmarkFailure("manifest contains no cases")
    workloads = {case.workload for case in cases}
    if any(workload not in workloads for workload in application_workloads):
        raise BenchmarkFailure(
            "manifest application_workloads contains an unknown workload"
        )
    return BenchmarkSuite(
        hashlib.sha256(payload).hexdigest(),
        tuple(cases),
        tuple(application_workloads),
    )


def _select_cases(
    cases: tuple[BenchmarkCase, ...],
    case_ids: Sequence[str] | None,
) -> tuple[BenchmarkCase, ...]:
    if case_ids is None:
        return cases
    requested = tuple(case_ids)
    if not requested:
        raise BenchmarkFailure("case selection is empty")
    if any(not isinstance(case_id, str) or not case_id for case_id in requested):
        raise BenchmarkFailure("case IDs must be nonempty strings")
    if len(set(requested)) != len(requested):
        raise BenchmarkFailure("case selection contains duplicate IDs")
    available = {case.case_id: case for case in cases}
    unknown = [case_id for case_id in requested if case_id not in available]
    if unknown:
        raise BenchmarkFailure(f"unknown benchmark case: {unknown[0]}")
    return tuple(available[case_id] for case_id in requested)


def _require_outcome(
    case: BenchmarkCase,
    phase: str,
    outcome: RunOutcome,
    retired_instructions: int | None = None,
) -> int:
    result = outcome.result
    prefix = f"{case.case_id} {phase}"
    if result.status != "exit":
        raise BenchmarkFailure(f"{prefix}: expected exit, got {result.status}")
    if result.exit_code != 0:
        raise BenchmarkFailure(
            f"{prefix}: expected exit code 0, got {result.exit_code}"
        )
    if outcome.output != case.expected_output:
        raise BenchmarkFailure(
            f"{prefix}: output differs (expected {len(case.expected_output)} bytes, "
            f"got {len(outcome.output)})"
        )
    if (
        retired_instructions is not None
        and result.retired_instructions != retired_instructions
    ):
        raise BenchmarkFailure(
            f"{prefix}: retired instruction count changed from "
            f"{retired_instructions} to {result.retired_instructions}"
        )
    return result.retired_instructions


def _run_fresh(
    executable: str | os.PathLike[str],
    case: BenchmarkCase,
    phase: str,
    timeout: float,
    *,
    timed: bool = False,
) -> tuple[RunOutcome, int | None]:
    try:
        with VmServer(executable, timeout=timeout) as server:
            server.load(case.elf)
            started = time.perf_counter_ns() if timed else None
            outcome = server.run(
                case.input_data,
                instruction_limit=case.instruction_limit,
                output_limit=len(case.expected_output),
            )
            elapsed = time.perf_counter_ns() - started if started is not None else None
            server.unload()
    except Exception as error:
        raise BenchmarkFailure(f"{case.case_id} {phase}: {error}") from error
    if elapsed is not None and elapsed <= 0:
        raise BenchmarkFailure(f"{case.case_id} {phase}: clock did not advance")
    return outcome, elapsed


def _measure_case(
    executable: str | os.PathLike[str],
    case: BenchmarkCase,
    warmups: int,
    repetitions: int,
    timeout: float,
) -> dict[str, object]:
    correctness, _ = _run_fresh(executable, case, "correctness run", timeout)
    retired = _require_outcome(case, "correctness run", correctness)
    for index in range(warmups):
        phase = f"warmup {index + 1}"
        outcome, _ = _run_fresh(executable, case, phase, timeout)
        _require_outcome(case, phase, outcome, retired)

    samples = []
    for index in range(repetitions):
        phase = f"repetition {index + 1}"
        outcome, elapsed = _run_fresh(executable, case, phase, timeout, timed=True)
        _require_outcome(case, phase, outcome, retired)
        assert elapsed is not None
        samples.append(elapsed)

    return {
        "id": case.case_id,
        "workload": case.workload,
        "retired_instructions": retired,
        "samples_ns": samples,
        "median_ns": statistics.median(samples),
    }


def _run_count(value: int, name: str, *, allow_zero: bool) -> int:
    minimum = 0 if allow_zero else 1
    if type(value) is not int or value < minimum:
        qualifier = "nonnegative" if allow_zero else "positive"
        raise BenchmarkFailure(f"{name} must be a {qualifier} integer")
    return value


def run_benchmark_suite(
    executable: str | os.PathLike[str],
    suite: BenchmarkSuite,
    *,
    warmups: int = DEFAULT_WARMUPS,
    repetitions: int = DEFAULT_REPETITIONS,
    timeout: float = DEFAULT_TIMEOUT,
) -> dict[str, object]:
    """Measure a loaded suite without rereading its manifest or artifacts."""

    warmups = _run_count(warmups, "warmups", allow_zero=True)
    repetitions = _run_count(repetitions, "repetitions", allow_zero=False)
    results = []
    for case in suite.cases:
        try:
            result = _measure_case(executable, case, warmups, repetitions, timeout)
        except BenchmarkFailure:
            raise
        except Exception as error:
            raise BenchmarkFailure(f"{case.case_id} serve: {error}") from error
        results.append(result)
    return {
        "schema_version": 1,
        "manifest_sha256": suite.sha256,
        "interface": "serve",
        "warmups": warmups,
        "repetitions": repetitions,
        "timeout_seconds": float(timeout),
        "cases": results,
    }


def run_benchmarks(
    executable: str | os.PathLike[str],
    manifest: str | os.PathLike[str] = DEFAULT_MANIFEST,
    *,
    warmups: int = DEFAULT_WARMUPS,
    repetitions: int = DEFAULT_REPETITIONS,
    timeout: float = DEFAULT_TIMEOUT,
    case_ids: Sequence[str] | None = None,
) -> dict[str, object]:
    """Load and measure each selected case in a fresh VM server."""

    # Preserve configuration-error precedence without loading an otherwise
    # unused suite.
    _run_count(warmups, "warmups", allow_zero=True)
    _run_count(repetitions, "repetitions", allow_zero=False)
    suite = load_benchmark_suite(manifest).select(case_ids)
    return run_benchmark_suite(
        executable,
        suite,
        warmups=warmups,
        repetitions=repetitions,
        timeout=timeout,
    )


def _json_text(result: dict[str, object]) -> str:
    return json.dumps(result, indent=2, sort_keys=True) + "\n"


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Measure RV32IM workloads through isolated serve processes."
    )
    parser.add_argument("vm", help="path to an rv32vm executable")
    parser.add_argument(
        "manifest",
        nargs="?",
        default=DEFAULT_MANIFEST,
        help=f"manifest path (default: {DEFAULT_MANIFEST})",
    )
    parser.add_argument(
        "--warmups",
        type=int,
        default=DEFAULT_WARMUPS,
        help=f"untimed warmup runs per case (default: {DEFAULT_WARMUPS})",
    )
    parser.add_argument(
        "--repetitions",
        type=int,
        default=DEFAULT_REPETITIONS,
        help=f"timed runs per case (default: {DEFAULT_REPETITIONS})",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=DEFAULT_TIMEOUT,
        help=f"timeout in seconds per VM operation (default: {DEFAULT_TIMEOUT:g})",
    )
    parser.add_argument(
        "--case",
        dest="case_ids",
        action="append",
        help="run only this case (repeatable)",
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="write result JSON to this file instead of standard output",
    )
    arguments = parser.parse_args(argv)

    try:
        result = run_benchmarks(
            arguments.vm,
            arguments.manifest,
            warmups=arguments.warmups,
            repetitions=arguments.repetitions,
            timeout=arguments.timeout,
            case_ids=arguments.case_ids,
        )
        text = _json_text(result)
        if arguments.output is None:
            print(text, end="")
        else:
            arguments.output.write_text(text, encoding="utf-8")
    except (BenchmarkFailure, OSError) as error:
        print(f"benchmark failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
