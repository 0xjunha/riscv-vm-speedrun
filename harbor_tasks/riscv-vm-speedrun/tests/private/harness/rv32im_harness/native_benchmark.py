"""Measure target-independent workload functions in native executables."""

from __future__ import annotations

import json
import math
import os
import stat
import statistics
import tempfile
from collections.abc import Sequence
from pathlib import Path

from .benchmark import (
    DEFAULT_MANIFEST,
    DEFAULT_REPETITIONS,
    DEFAULT_TIMEOUT,
    DEFAULT_WARMUPS,
    BenchmarkCase,
    BenchmarkFailure,
    BenchmarkSuite,
    _run_count,
    load_benchmark_suite,
)
from .vm_client import VmError, VmTimeout, _diagnostic, _run_command

_RESULT_KEYS = {"schema_version", "output_hex", "samples_ns"}


def _positive_timeout(value: float) -> float:
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not math.isfinite(value)
        or value <= 0
    ):
        raise BenchmarkFailure("timeout must be a positive finite number")
    return float(value)


def _executable(directory: Path, workload: str) -> Path:
    path = (directory / workload).resolve()
    if not path.is_relative_to(directory):
        raise BenchmarkFailure(f"{workload}: native executable leaves its directory")
    try:
        metadata = path.stat()
    except OSError as error:
        raise BenchmarkFailure(
            f"{workload}: cannot inspect native executable: {error}"
        ) from error
    if not stat.S_ISREG(metadata.st_mode) or not os.access(path, os.X_OK):
        raise BenchmarkFailure(f"{workload}: native executable is not executable")
    return path


def _measure_case(
    directory: Path,
    case: BenchmarkCase,
    warmups: int,
    repetitions: int,
    timeout: float,
) -> dict[str, object]:
    executable = _executable(directory, case.workload)
    total_timeout = timeout * (1 + warmups + repetitions)
    try:
        with tempfile.TemporaryDirectory(prefix="rv32im-native-") as temporary:
            completed = _run_command(
                executable,
                [str(warmups), str(repetitions)],
                cwd=temporary,
                timeout=total_timeout,
                input_data=case.input_data,
            )
    except VmTimeout as error:
        raise BenchmarkFailure(
            f"{case.case_id}: native executable exceeded {total_timeout:g} seconds"
        ) from error
    except (OSError, VmError) as error:
        raise BenchmarkFailure(
            f"{case.case_id}: failed to start native executable: {error}"
        ) from error

    if completed.returncode != 0:
        detail = _diagnostic(completed.stderr)
        suffix = f": {detail}" if detail else ""
        raise BenchmarkFailure(
            f"{case.case_id}: native executable exited with "
            f"status {completed.returncode}{suffix}"
        )
    try:
        document = json.loads(completed.stdout)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise BenchmarkFailure(
            f"{case.case_id}: native result is not valid JSON"
        ) from error
    if (
        not isinstance(document, dict)
        or set(document) != _RESULT_KEYS
        or document.get("schema_version") != 1
    ):
        raise BenchmarkFailure(f"{case.case_id}: native result schema is invalid")

    output_hex = document["output_hex"]
    if (
        not isinstance(output_hex, str)
        or len(output_hex) % 2
        or any(character not in "0123456789abcdef" for character in output_hex)
    ):
        raise BenchmarkFailure(f"{case.case_id}: native output is invalid")
    output = bytes.fromhex(output_hex)
    if output != case.expected_output:
        raise BenchmarkFailure(f"{case.case_id}: native output differs")

    samples = document["samples_ns"]
    if (
        not isinstance(samples, list)
        or len(samples) != repetitions
        or any(type(sample) is not int or sample <= 0 for sample in samples)
    ):
        raise BenchmarkFailure(f"{case.case_id}: native samples are invalid")
    return {
        "id": case.case_id,
        "workload": case.workload,
        "samples_ns": samples,
        "median_ns": statistics.median(samples),
    }


def _native_configuration(
    directory: str | os.PathLike[str],
    warmups: int,
    repetitions: int,
    timeout: float,
) -> tuple[Path, int, int, float]:
    warmups = _run_count(warmups, "warmups", allow_zero=True)
    repetitions = _run_count(repetitions, "repetitions", allow_zero=False)
    timeout = _positive_timeout(timeout)
    root = Path(directory).expanduser().resolve()
    if not root.is_dir():
        raise BenchmarkFailure(f"native executable directory is invalid: {root}")
    return root, warmups, repetitions, timeout


def _run_native_benchmark_suite(
    root: Path,
    suite: BenchmarkSuite,
    warmups: int,
    repetitions: int,
    timeout: float,
) -> dict[str, object]:

    results = [
        _measure_case(root, case, warmups, repetitions, timeout) for case in suite.cases
    ]
    return {
        "schema_version": 1,
        "manifest_sha256": suite.sha256,
        "interface": "native",
        "warmups": warmups,
        "repetitions": repetitions,
        "timeout_seconds": timeout,
        "cases": results,
    }


def run_native_benchmark_suite(
    directory: str | os.PathLike[str],
    suite: BenchmarkSuite,
    *,
    warmups: int = DEFAULT_WARMUPS,
    repetitions: int = DEFAULT_REPETITIONS,
    timeout: float = DEFAULT_TIMEOUT,
) -> dict[str, object]:
    """Measure native executables for a loaded benchmark suite."""

    root, warmups, repetitions, timeout = _native_configuration(
        directory, warmups, repetitions, timeout
    )
    return _run_native_benchmark_suite(root, suite, warmups, repetitions, timeout)


def run_native_benchmarks(
    directory: str | os.PathLike[str],
    manifest: str | os.PathLike[str] = DEFAULT_MANIFEST,
    *,
    warmups: int = DEFAULT_WARMUPS,
    repetitions: int = DEFAULT_REPETITIONS,
    timeout: float = DEFAULT_TIMEOUT,
    case_ids: Sequence[str] | None = None,
) -> dict[str, object]:
    """Load and measure native executables for the selected workloads."""

    root, warmups, repetitions, timeout = _native_configuration(
        directory, warmups, repetitions, timeout
    )
    suite = load_benchmark_suite(manifest).select(case_ids)
    return _run_native_benchmark_suite(
        root,
        suite,
        warmups,
        repetitions,
        timeout,
    )
