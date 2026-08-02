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
from .native_benchmark import run_native_benchmarks

_MATCHING_RUN_FIELDS = (
    "schema_version",
    "manifest_sha256",
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


def _labeled_path(value: str, kind: str) -> tuple[str, str]:
    label, separator, path = value.partition("=")
    if not separator or not _valid_label(label) or not path:
        raise argparse.ArgumentTypeError(f"{kind} must be LABEL=PATH")
    return label, path


def _vm_spec(value: str) -> tuple[str, str]:
    return _labeled_path(value, "VM")


def _native_spec(value: str) -> tuple[str, str]:
    return _labeled_path(value, "native reference")


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
    implementation: str,
    implementation_result: dict[str, object],
) -> list[dict[str, object]]:
    for field in _MATCHING_RUN_FIELDS:
        if baseline_result.get(field) != implementation_result.get(field):
            raise BenchmarkFailure(f"{implementation} run disagrees on {field}")

    baseline_cases = baseline_result.get("cases")
    implementation_cases = implementation_result.get("cases")
    if not isinstance(baseline_cases, list) or not isinstance(
        implementation_cases, list
    ):
        raise BenchmarkFailure("benchmark run cases are invalid")
    if len(baseline_cases) != len(implementation_cases):
        raise BenchmarkFailure(f"{implementation} run contains a different case count")

    interface = implementation_result.get("interface")
    if interface not in {"serve", "native"}:
        raise BenchmarkFailure(f"{implementation} run has an invalid interface")

    comparisons = []
    for index, (baseline_case, implementation_case) in enumerate(
        zip(baseline_cases, implementation_cases, strict=True)
    ):
        if not isinstance(baseline_case, dict) or not isinstance(
            implementation_case, dict
        ):
            raise BenchmarkFailure(f"benchmark case {index} is invalid")
        for field in ("id", "workload"):
            if baseline_case.get(field) != implementation_case.get(field):
                raise BenchmarkFailure(
                    f"{implementation} run disagrees on case {index} {field}"
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
        if (
            interface == "serve"
            and implementation_case.get("retired_instructions") != retired
        ):
            raise BenchmarkFailure(
                f"{implementation} run disagrees on case {index} retired_instructions"
            )
        baseline_median = _positive_median(
            baseline_case.get("median_ns"), f"baseline {case_id}"
        )
        implementation_median = _positive_median(
            implementation_case.get("median_ns"), f"{implementation} {case_id}"
        )
        comparisons.append(
            {
                "implementation": implementation,
                "id": case_id,
                "workload": workload,
                "retired_instructions": retired,
                "baseline_median_ns": baseline_median,
                "implementation_median_ns": implementation_median,
                "speedup": baseline_median / implementation_median,
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
    native: tuple[str, str | os.PathLike[str]] | None = None,
) -> dict[str, object]:
    """Run VMs and an optional native reference with shared settings."""

    records = _validated_executables(executables, baseline)
    native_record = None
    if native is not None:
        label, directory = native
        if not _valid_label(label):
            raise BenchmarkFailure(f"invalid native reference label: {label!r}")
        if label in executables:
            raise BenchmarkFailure(f"duplicate implementation label: {label}")
        path = os.fspath(directory)
        if not isinstance(path, str) or not path:
            raise BenchmarkFailure(f"{label}: native reference path is invalid")
        native_record = (label, path)

    selected_cases = None if case_ids is None else tuple(case_ids)
    runs = {}
    implementations = {}
    for label, executable in records:
        implementations[label] = {"interface": "serve", "path": executable}
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

    if native_record is not None:
        label, directory = native_record
        implementations[label] = {"interface": "native", "path": directory}
        try:
            runs[label] = run_native_benchmarks(
                directory,
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
    for label, run in runs.items():
        if label != baseline:
            comparisons.extend(_comparisons(baseline_result, label, run))

    return {
        "schema_version": 1,
        "baseline": baseline,
        "implementations": implementations,
        "runs": runs,
        "comparisons": comparisons,
    }


def _json_text(result: dict[str, object]) -> str:
    return json.dumps(result, indent=2, sort_keys=True) + "\n"


def _table_text(headers: tuple[str, ...], rows: Sequence[tuple[str, ...]]) -> str:
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


def _run_case_medians(
    result: dict[str, object], implementation: str
) -> tuple[tuple[str, ...], dict[str, int | float]]:
    runs = result.get("runs")
    if not isinstance(runs, Mapping):
        raise BenchmarkFailure("benchmark comparison runs are invalid")
    run = runs.get(implementation)
    cases = run.get("cases") if isinstance(run, Mapping) else None
    if not isinstance(cases, list):
        raise BenchmarkFailure(f"{implementation} benchmark cases are invalid")

    case_ids = []
    medians = {}
    for index, case in enumerate(cases):
        case_id = case.get("id") if isinstance(case, Mapping) else None
        if not isinstance(case_id, str) or not case_id or case_id in medians:
            raise BenchmarkFailure(
                f"{implementation} benchmark case {index} is invalid"
            )
        case_ids.append(case_id)
        medians[case_id] = _positive_median(
            case.get("median_ns"), f"{implementation} {case_id}"
        )
    return tuple(case_ids), medians


def _geometric_mean(values: Sequence[float]) -> float:
    if not values:
        raise BenchmarkFailure("geometric mean requires at least one value")
    return math.exp(math.fsum(math.log(value) for value in values) / len(values))


def _application_summary_text(
    result: dict[str, object], application_case_ids: Sequence[str]
) -> str | None:
    requested = tuple(application_case_ids)
    if not requested:
        return None
    if any(not isinstance(case_id, str) or not case_id for case_id in requested):
        raise BenchmarkFailure("application case IDs must be nonempty strings")
    if len(set(requested)) != len(requested):
        raise BenchmarkFailure("application case selection contains duplicate IDs")

    baseline = str(result["baseline"])
    baseline_case_ids, baseline_medians = _run_case_medians(result, baseline)
    selected = tuple(case_id for case_id in requested if case_id in baseline_medians)
    if not selected:
        return None

    implementations = result.get("implementations")
    if not isinstance(implementations, Mapping):
        raise BenchmarkFailure("benchmark comparison implementations are invalid")
    native_labels = [
        str(label)
        for label, record in implementations.items()
        if isinstance(record, Mapping) and record.get("interface") == "native"
    ]
    if len(native_labels) != 1:
        raise BenchmarkFailure(
            "application aggregate requires exactly one native reference"
        )
    native = native_labels[0]
    native_case_ids, native_medians = _run_case_medians(result, native)
    if native_case_ids != baseline_case_ids:
        raise BenchmarkFailure("native run contains different benchmark cases")

    implementation_labels = [baseline]
    implementation_labels.extend(
        str(label)
        for label, record in implementations.items()
        if str(label) != baseline
        and not (isinstance(record, Mapping) and record.get("interface") == "native")
    )
    implementation_labels.append(native)

    rows = []
    for implementation in implementation_labels:
        case_ids, medians = _run_case_medians(result, implementation)
        if case_ids != baseline_case_ids:
            raise BenchmarkFailure(
                f"{implementation} run contains different benchmark cases"
            )
        speedup = _geometric_mean(
            [baseline_medians[case_id] / medians[case_id] for case_id in selected]
        )
        native_fraction = _geometric_mean(
            [native_medians[case_id] / medians[case_id] for case_id in selected]
        )
        rows.append(
            (
                implementation,
                f"{speedup:.3f}x",
                f"{native_fraction * 100:.4f}%",
                f"{1 / native_fraction:.3f}x",
            )
        )

    headers = (
        "implementation",
        f"speedup vs {baseline}",
        f"{native} performance",
        f"time vs {native}",
    )
    excluded = tuple(
        case_id for case_id in baseline_case_ids if case_id not in set(selected)
    )
    description = (
        f"geometric mean across {len(selected)} application "
        f"{'workload' if len(selected) == 1 else 'workloads'}"
    )
    lines = ["application aggregate", description]
    if excluded:
        lines.append(f"excluded cases: {', '.join(excluded)}")
    lines.extend(("", _table_text(headers, rows)))
    return "\n".join(lines)


def _summary_text(
    result: dict[str, object], application_case_ids: Sequence[str] = ()
) -> str:
    baseline = str(result["baseline"])
    comparisons = result["comparisons"]
    assert isinstance(comparisons, list)
    rows = [
        (
            str(record["implementation"]),
            str(record["workload"]),
            str(record["baseline_median_ns"]),
            str(record["implementation_median_ns"]),
            f"{record['speedup']:.3f}x",
        )
        for record in comparisons
    ]
    headers = (
        "implementation",
        "workload",
        f"{baseline} median (ns)",
        "implementation median (ns)",
        "speedup",
    )
    detailed = _table_text(headers, rows)
    application = _application_summary_text(result, application_case_ids)
    if application is None:
        return detailed
    return f"{detailed}\n\n{application}"


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
        "--native",
        type=_native_spec,
        metavar="LABEL=DIRECTORY",
        help="add host-native workload executables from a directory",
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
        "--application-case",
        dest="application_case_ids",
        action="append",
        default=[],
        help=(
            "include this case in the native-normalized application aggregate "
            "(repeatable)"
        ),
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
            native=arguments.native,
        )
        summary = _summary_text(result, arguments.application_case_ids)
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        arguments.output.write_text(_json_text(result), encoding="utf-8")
    except (BenchmarkFailure, OSError) as error:
        print(f"benchmark comparison failed: {error}", file=sys.stderr)
        return 1

    print(summary)
    print(f"\nraw results: {arguments.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
