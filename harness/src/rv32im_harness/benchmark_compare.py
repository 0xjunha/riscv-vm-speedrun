"""Compare labeled VMs with identical public benchmark settings."""

from __future__ import annotations

import argparse
import json
import math
import os
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path

from .benchmark import (
    DEFAULT_MANIFEST,
    DEFAULT_REPETITIONS,
    DEFAULT_TIMEOUT,
    DEFAULT_WARMUPS,
    BenchmarkFailure,
    run_benchmarks,
)

_MATCHING_RUN_FIELDS = (
    "schema_version",
    "manifest_sha256",
    "interface",
    "warmups",
    "repetitions",
    "timeout_seconds",
)


def _valid_label(label: object) -> bool:
    return (
        isinstance(label, str)
        and bool(label)
        and label.isascii()
        and label[0].isalnum()
        and all(character.isalnum() or character in "._-" for character in label)
    )


def _vm_spec(value: str) -> tuple[str, str]:
    label, separator, executable = value.partition("=")
    if not separator or not _valid_label(label) or not executable:
        raise argparse.ArgumentTypeError("VM must be LABEL=EXECUTABLE")
    return label, executable


def _vm_mapping(specs: Sequence[tuple[str, str]]) -> dict[str, str]:
    executables = {}
    for label, executable in specs:
        if label in executables:
            raise BenchmarkFailure(f"duplicate VM label: {label}")
        executables[label] = executable
    return executables


def _validated_executables(
    executables: Mapping[str, str | os.PathLike[str]],
    baseline: str,
) -> tuple[tuple[str, str], ...]:
    records = []
    for label, executable in executables.items():
        if not _valid_label(label):
            raise BenchmarkFailure(f"invalid VM label: {label!r}")
        path = os.fspath(executable)
        if not isinstance(path, str) or not path:
            raise BenchmarkFailure(f"{label}: executable path is invalid")
        records.append((label, path))
    if len(records) < 2:
        raise BenchmarkFailure("at least two VMs are required")
    if baseline not in executables:
        raise BenchmarkFailure(f"baseline VM is not defined: {baseline}")
    return tuple(records)


def _positive_median(value: object, label: str) -> int | float:
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not math.isfinite(value)
        or value <= 0
    ):
        raise BenchmarkFailure(f"{label} median_ns is invalid")
    return value


def _comparisons(
    baseline_result: dict[str, object],
    candidate_label: str,
    candidate_result: dict[str, object],
) -> list[dict[str, object]]:
    for field in _MATCHING_RUN_FIELDS:
        if baseline_result.get(field) != candidate_result.get(field):
            raise BenchmarkFailure(f"{candidate_label} run disagrees on {field}")

    baseline_cases = baseline_result.get("cases")
    candidate_cases = candidate_result.get("cases")
    if not isinstance(baseline_cases, list) or not isinstance(candidate_cases, list):
        raise BenchmarkFailure("benchmark run cases are invalid")
    if len(baseline_cases) != len(candidate_cases):
        raise BenchmarkFailure(f"{candidate_label} run contains a different case count")

    comparisons = []
    for index, (baseline_case, candidate_case) in enumerate(
        zip(baseline_cases, candidate_cases, strict=True)
    ):
        if not isinstance(baseline_case, dict) or not isinstance(candidate_case, dict):
            raise BenchmarkFailure(f"benchmark case {index} is invalid")
        for field in ("id", "workload", "retired_instructions"):
            if baseline_case.get(field) != candidate_case.get(field):
                raise BenchmarkFailure(
                    f"{candidate_label} run disagrees on case {index} {field}"
                )
        case_id = baseline_case.get("id")
        workload = baseline_case.get("workload")
        retired = baseline_case.get("retired_instructions")
        if (
            not isinstance(case_id, str)
            or not case_id
            or not isinstance(workload, str)
            or not workload
            or type(retired) is not int
            or retired < 0
        ):
            raise BenchmarkFailure(f"benchmark case {index} metadata is invalid")
        baseline_median = _positive_median(
            baseline_case.get("median_ns"), f"baseline {case_id}"
        )
        candidate_median = _positive_median(
            candidate_case.get("median_ns"), f"{candidate_label} {case_id}"
        )
        comparisons.append(
            {
                "candidate": candidate_label,
                "id": case_id,
                "workload": workload,
                "retired_instructions": retired,
                "baseline_median_ns": baseline_median,
                "candidate_median_ns": candidate_median,
                "speedup": baseline_median / candidate_median,
            }
        )
    return comparisons


def run_comparison(
    executables: Mapping[str, str | os.PathLike[str]],
    baseline: str,
    manifest: str | os.PathLike[str] = DEFAULT_MANIFEST,
    *,
    warmups: int = DEFAULT_WARMUPS,
    repetitions: int = DEFAULT_REPETITIONS,
    timeout: float = DEFAULT_TIMEOUT,
    case_ids: Sequence[str] | None = None,
) -> dict[str, object]:
    """Run labeled VMs and retain complete measurements and baseline comparisons."""

    records = _validated_executables(executables, baseline)
    selected_cases = None if case_ids is None else tuple(case_ids)
    runs = {}
    for label, executable in records:
        try:
            runs[label] = run_benchmarks(
                executable,
                manifest,
                warmups=warmups,
                repetitions=repetitions,
                timeout=timeout,
                case_ids=selected_cases,
            )
        except BenchmarkFailure as error:
            raise BenchmarkFailure(f"{label}: {error}") from error

    baseline_result = runs[baseline]
    comparisons = []
    for label, _executable in records:
        if label != baseline:
            comparisons.extend(_comparisons(baseline_result, label, runs[label]))

    return {
        "schema_version": 1,
        "baseline": baseline,
        "executables": dict(records),
        "runs": runs,
        "comparisons": comparisons,
    }


def _json_text(result: dict[str, object]) -> str:
    return json.dumps(result, indent=2, sort_keys=True) + "\n"


def _summary_text(result: dict[str, object]) -> str:
    baseline = str(result["baseline"])
    comparisons = result["comparisons"]
    assert isinstance(comparisons, list)
    rows = [
        (
            str(record["candidate"]),
            str(record["workload"]),
            str(record["baseline_median_ns"]),
            str(record["candidate_median_ns"]),
            f"{record['speedup']:.3f}x",
        )
        for record in comparisons
    ]
    headers = (
        "candidate",
        "workload",
        f"{baseline} median (ns)",
        "candidate median (ns)",
        "speedup",
    )
    widths = [
        max(len(headers[index]), *(len(row[index]) for row in rows))
        for index in range(len(headers))
    ]

    def line(values: tuple[str, ...]) -> str:
        return "  ".join(
            value.ljust(widths[index]) for index, value in enumerate(values)
        ).rstrip()

    return "\n".join(
        (
            line(headers),
            line(tuple("-" * width for width in widths)),
            *(line(row) for row in rows),
        )
    )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Run labeled RV32IM VMs and compare them with one baseline."
    )
    parser.add_argument(
        "manifest",
        nargs="?",
        default=DEFAULT_MANIFEST,
        help=f"manifest path (default: {DEFAULT_MANIFEST})",
    )
    parser.add_argument(
        "--vm",
        dest="vms",
        action="append",
        type=_vm_spec,
        required=True,
        metavar="LABEL=EXECUTABLE",
        help="add a labeled rv32vm executable (repeatable)",
    )
    parser.add_argument("--baseline", required=True, help="label of the baseline VM")
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
        required=True,
        help="write raw measurements and comparisons to this JSON file",
    )
    arguments = parser.parse_args(argv)

    try:
        result = run_comparison(
            _vm_mapping(arguments.vms),
            arguments.baseline,
            arguments.manifest,
            warmups=arguments.warmups,
            repetitions=arguments.repetitions,
            timeout=arguments.timeout,
            case_ids=arguments.case_ids,
        )
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        arguments.output.write_text(_json_text(result), encoding="utf-8")
    except (BenchmarkFailure, OSError) as error:
        print(f"benchmark comparison failed: {error}", file=sys.stderr)
        return 1

    print(_summary_text(result))
    print(f"\nraw results: {arguments.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
