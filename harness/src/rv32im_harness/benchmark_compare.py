"""Compare labeled VMs with identical public benchmark settings."""

from __future__ import annotations

import argparse
import json
import math
import os
import sys
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

from .benchmark import (
    DEFAULT_MANIFEST,
    DEFAULT_REPETITIONS,
    DEFAULT_TIMEOUT,
    DEFAULT_WARMUPS,
    BenchmarkFailure,
    load_benchmark_suite,
    run_benchmark_suite,
)
from .native_benchmark import run_native_benchmark_suite

_MATCHING_RUN_FIELDS = (
    "schema_version",
    "manifest_sha256",
    "warmups",
    "repetitions",
    "timeout_seconds",
)
_CHUNK_RUN_FIELDS = (*_MATCHING_RUN_FIELDS, "interface")


@dataclass(frozen=True)
class _AggregateContext:
    baseline: str
    native: str
    labels: tuple[str, ...]
    case_ids: tuple[str, ...]
    workloads: Mapping[str, str]
    medians: Mapping[str, Mapping[str, int | float]]


@dataclass(frozen=True)
class _Participant:
    label: str
    interface: str
    path: str


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


def _append_case_run(
    runs: dict[str, dict[str, object]],
    label: str,
    result: dict[str, object],
    case_id: str,
    workload: str,
    interface: str,
) -> None:
    if result.get("interface") != interface:
        raise BenchmarkFailure(f"{case_id} run has an invalid interface")
    cases = result.get("cases")
    if not isinstance(cases, list) or len(cases) != 1:
        raise BenchmarkFailure(f"{case_id} run must contain exactly one case")
    measured = cases[0]
    if not isinstance(measured, dict):
        raise BenchmarkFailure(f"{case_id} run case is invalid")
    if measured.get("id") != case_id:
        raise BenchmarkFailure(f"{case_id} run returned a different case ID")
    if measured.get("workload") != workload:
        raise BenchmarkFailure(f"{case_id} run returned a different workload")

    assembled = runs.get(label)
    if assembled is None:
        runs[label] = {**result, "cases": [measured]}
        return
    for field in _CHUNK_RUN_FIELDS:
        if assembled.get(field) != result.get(field):
            raise BenchmarkFailure(f"{case_id} run disagrees on {field}")
    assembled_cases = assembled.get("cases")
    if not isinstance(assembled_cases, list):
        raise BenchmarkFailure(f"{case_id} assembled run cases are invalid")
    assembled_cases.append(measured)


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
    suite = load_benchmark_suite(manifest).select(selected_cases)
    runs: dict[str, dict[str, object]] = {}
    participants = [
        _Participant(label, "serve", executable) for label, executable in records
    ]
    if native_record is not None:
        label, directory = native_record
        participants.append(_Participant(label, "native", directory))
    participant_records = tuple(participants)
    implementations = {
        participant.label: {
            "interface": participant.interface,
            "path": participant.path,
        }
        for participant in participant_records
    }

    for case_index, case in enumerate(suite.cases):
        case_suite = suite.select((case.case_id,))
        rotation = case_index % len(participant_records)
        ordered_participants = (
            participant_records[rotation:] + participant_records[:rotation]
        )
        for participant in ordered_participants:
            try:
                if participant.interface == "serve":
                    result = run_benchmark_suite(
                        participant.path,
                        case_suite,
                        warmups=warmups,
                        repetitions=repetitions,
                        timeout=timeout,
                    )
                else:
                    result = run_native_benchmark_suite(
                        participant.path,
                        case_suite,
                        warmups=warmups,
                        repetitions=repetitions,
                        timeout=timeout,
                    )
                _append_case_run(
                    runs,
                    participant.label,
                    result,
                    case.case_id,
                    case.workload,
                    participant.interface,
                )
            except BenchmarkFailure as error:
                raise BenchmarkFailure(f"{participant.label}: {error}") from error

    baseline_result = runs[baseline]
    comparisons = []
    for label, run in runs.items():
        if label != baseline:
            comparisons.extend(_comparisons(baseline_result, label, run))

    return {
        "schema_version": 1,
        "baseline": baseline,
        "implementations": implementations,
        "schedule": {
            "strategy": "case_interleaved_rotating_participants",
            "case_order": [case.case_id for case in suite.cases],
            "initial_participant_order": [
                participant.label for participant in participant_records
            ],
            "rotation": "left_by_case_index",
        },
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


def _run_case_data(
    result: dict[str, object], implementation: str
) -> tuple[tuple[str, ...], dict[str, str], dict[str, int | float]]:
    runs = result.get("runs")
    if not isinstance(runs, Mapping):
        raise BenchmarkFailure("benchmark comparison runs are invalid")
    run = runs.get(implementation)
    cases = run.get("cases") if isinstance(run, Mapping) else None
    if not isinstance(cases, list):
        raise BenchmarkFailure(f"{implementation} benchmark cases are invalid")

    case_ids = []
    workloads = {}
    medians = {}
    for index, case in enumerate(cases):
        case_id = case.get("id") if isinstance(case, Mapping) else None
        workload = case.get("workload") if isinstance(case, Mapping) else None
        if (
            not isinstance(case_id, str)
            or not case_id
            or case_id in medians
            or not isinstance(workload, str)
            or not workload
        ):
            raise BenchmarkFailure(
                f"{implementation} benchmark case {index} is invalid"
            )
        case_ids.append(case_id)
        workloads[case_id] = workload
        medians[case_id] = _positive_median(
            case.get("median_ns"), f"{implementation} {case_id}"
        )
    return tuple(case_ids), workloads, medians


def _geometric_mean(values: Sequence[float]) -> float:
    if not values:
        raise BenchmarkFailure("geometric mean requires at least one value")
    return math.exp(math.fsum(math.log(value) for value in values) / len(values))


def _aggregate_context(result: dict[str, object], report: str) -> _AggregateContext:
    baseline = str(result["baseline"])
    implementations = result.get("implementations")
    if not isinstance(implementations, Mapping):
        raise BenchmarkFailure("benchmark comparison implementations are invalid")
    native_labels = [
        str(label)
        for label, record in implementations.items()
        if isinstance(record, Mapping) and record.get("interface") == "native"
    ]
    if len(native_labels) != 1:
        raise BenchmarkFailure(f"{report} requires exactly one native reference")
    native = native_labels[0]
    labels = tuple(
        str(label)
        for label, record in implementations.items()
        if not (isinstance(record, Mapping) and record.get("interface") == "native")
    ) + (native,)

    case_ids, workloads, baseline_medians = _run_case_data(result, baseline)
    medians_by_implementation = {baseline: baseline_medians}
    for implementation in labels:
        if implementation == baseline:
            continue
        implementation_case_ids, implementation_workloads, medians = _run_case_data(
            result, implementation
        )
        if implementation_case_ids != case_ids:
            raise BenchmarkFailure(
                f"{implementation} run contains different benchmark cases"
            )
        if implementation_workloads != workloads:
            raise BenchmarkFailure(
                f"{implementation} run contains different workload metadata"
            )
        medians_by_implementation[implementation] = medians
    return _AggregateContext(
        baseline, native, labels, case_ids, workloads, medians_by_implementation
    )


def _aggregate_ratios(
    aggregate: _AggregateContext,
    implementation: str,
    case_ids: Sequence[str],
) -> tuple[float, float]:
    medians = aggregate.medians
    return (
        _geometric_mean(
            [
                medians[aggregate.baseline][case_id] / medians[implementation][case_id]
                for case_id in case_ids
            ]
        ),
        _geometric_mean(
            [
                medians[aggregate.native][case_id] / medians[implementation][case_id]
                for case_id in case_ids
            ]
        ),
    )


def _validated_application_selection(
    values: Sequence[str], kind: str
) -> tuple[str, ...]:
    requested = tuple(values)
    if any(not isinstance(value, str) or not value for value in requested):
        raise BenchmarkFailure(f"application {kind}s must be nonempty strings")
    duplicates = tuple(
        dict.fromkeys(value for value in requested if requested.count(value) > 1)
    )
    if duplicates:
        raise BenchmarkFailure(
            f"application {kind} selection contains duplicates: {', '.join(duplicates)}"
        )
    return requested


def _application_workload_cases(
    aggregate: _AggregateContext,
    application_case_ids: Sequence[str],
    application_workloads: Sequence[str],
) -> tuple[tuple[str, tuple[str, ...]], ...]:
    requested_cases = _validated_application_selection(application_case_ids, "case ID")
    requested_workloads = _validated_application_selection(
        application_workloads, "workload"
    )
    if requested_cases and requested_workloads:
        raise BenchmarkFailure(
            "application aggregate cannot mix case and workload selections"
        )
    if not requested_cases and not requested_workloads:
        return ()

    if requested_workloads:
        available_workloads = set(aggregate.workloads.values())
        unknown = tuple(
            workload
            for workload in requested_workloads
            if workload not in available_workloads
        )
        if unknown:
            raise BenchmarkFailure(
                f"unknown application workloads: {', '.join(unknown)}"
            )
        return tuple(
            (
                workload,
                tuple(
                    case_id
                    for case_id in aggregate.case_ids
                    if aggregate.workloads[case_id] == workload
                ),
            )
            for workload in requested_workloads
        )

    available_cases = set(aggregate.case_ids)
    unknown = tuple(
        case_id for case_id in requested_cases if case_id not in available_cases
    )
    if unknown:
        raise BenchmarkFailure(f"unknown application case IDs: {', '.join(unknown)}")
    grouped: dict[str, list[str]] = {}
    for case_id in requested_cases:
        grouped.setdefault(aggregate.workloads[case_id], []).append(case_id)
    return tuple((workload, tuple(case_ids)) for workload, case_ids in grouped.items())


def _workload_balanced_ratios(
    aggregate: _AggregateContext,
    implementation: str,
    workload_cases: Sequence[tuple[str, tuple[str, ...]]],
) -> tuple[float, float]:
    workload_ratios = [
        _aggregate_ratios(aggregate, implementation, case_ids)
        for _, case_ids in workload_cases
    ]
    return (
        _geometric_mean([speedup for speedup, _ in workload_ratios]),
        _geometric_mean([native_fraction for _, native_fraction in workload_ratios]),
    )


def _application_summary_text(
    result: dict[str, object],
    application_case_ids: Sequence[str] = (),
    application_workloads: Sequence[str] = (),
) -> str | None:
    if not application_case_ids and not application_workloads:
        return None

    aggregate = _aggregate_context(result, "application aggregate")
    workload_cases = _application_workload_cases(
        aggregate, application_case_ids, application_workloads
    )
    implementation_labels = (
        aggregate.baseline,
        *(
            label
            for label in aggregate.labels
            if label not in (aggregate.baseline, aggregate.native)
        ),
        aggregate.native,
    )

    rows = []
    for implementation in implementation_labels:
        speedup, native_fraction = _workload_balanced_ratios(
            aggregate, implementation, workload_cases
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
        f"speedup vs {aggregate.baseline}",
        f"{aggregate.native} performance",
        f"time vs {aggregate.native}",
    )
    selected = {
        case_id
        for _, workload_case_ids in workload_cases
        for case_id in workload_case_ids
    }
    excluded = tuple(
        case_id for case_id in aggregate.case_ids if case_id not in selected
    )
    case_count = sum(len(case_ids) for _, case_ids in workload_cases)
    workload_count = len(workload_cases)
    description = (
        f"geometric mean across {workload_count} application "
        f"{'workload' if workload_count == 1 else 'workloads'}"
    )
    if case_count != workload_count:
        description += (
            f", after geometric means within each workload ({case_count} cases total)"
        )
    lines = ["application aggregate", description]
    if excluded:
        lines.append(f"excluded cases: {', '.join(excluded)}")
    lines.extend(("", _table_text(headers, rows)))
    return "\n".join(lines)


def _horizon_cases(case_ids: Sequence[str]) -> tuple[tuple[int, tuple[str, ...]], ...]:
    groups: dict[int, dict[str, str]] = {}
    for case_id in case_ids:
        base, separator, suffix = case_id.rpartition("-")
        horizon = suffix.removesuffix("x")
        if (
            not separator
            or not base
            or not suffix.endswith("x")
            or not horizon.isdecimal()
            or horizon.startswith("0")
        ):
            raise BenchmarkFailure("horizon case IDs must end in -Nx")
        groups.setdefault(int(horizon), {})[base] = case_id

    cohorts = {frozenset(group) for group in groups.values()}
    if not groups or len(cohorts) != 1:
        raise BenchmarkFailure("horizons contain different benchmark cases")
    return tuple(
        (horizon, tuple(group.values())) for horizon, group in sorted(groups.items())
    )


def _horizon_summary_text(result: dict[str, object]) -> str:
    aggregate = _aggregate_context(result, "horizon report")
    horizon_cases = _horizon_cases(aggregate.case_ids)

    rows = []
    workload_count = len(horizon_cases[0][1])
    for horizon, selected in horizon_cases:
        for implementation in aggregate.labels:
            speedup, native_fraction = _aggregate_ratios(
                aggregate,
                implementation,
                selected,
            )
            rows.append(
                (
                    f"{horizon}x",
                    implementation,
                    f"{speedup:.3f}x",
                    f"{native_fraction * 100:.4f}%",
                    f"{1 / native_fraction:.3f}x",
                )
            )

    headers = (
        "horizon",
        "implementation",
        f"speedup vs {aggregate.baseline}",
        f"{aggregate.native} performance",
        f"time vs {aggregate.native}",
    )
    return "\n".join(
        (
            "long-workload aggregate",
            f"geometric mean across {workload_count} application workloads per horizon",
            "",
            _table_text(headers, rows),
        )
    )


def _summary_text(
    result: dict[str, object],
    application_case_ids: Sequence[str] = (),
    application_workloads: Sequence[str] = (),
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
    application = _application_summary_text(
        result, application_case_ids, application_workloads
    )
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
        "--application-workload",
        dest="application_workloads",
        action="append",
        default=[],
        help=(
            "include every measured case for this workload in the workload-balanced "
            "native-normalized application aggregate (repeatable)"
        ),
    )
    parser.add_argument(
        "--horizon-report",
        action="store_true",
        help="group the aggregate by -Nx case suffix",
    )
    parser.add_argument(
        "--output",
        type=Path,
        required=True,
        help="write raw measurements and comparisons to this JSON file",
    )
    arguments = parser.parse_args(argv)

    try:
        if arguments.application_case_ids and arguments.application_workloads:
            raise BenchmarkFailure(
                "--application-case conflicts with --application-workload"
            )
        if arguments.horizon_report and (
            arguments.application_case_ids or arguments.application_workloads
        ):
            raise BenchmarkFailure(
                "--horizon-report conflicts with application aggregate selectors"
            )
        _validated_application_selection(arguments.application_case_ids, "case ID")
        _validated_application_selection(arguments.application_workloads, "workload")
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
        summary = (
            _horizon_summary_text(result)
            if arguments.horizon_report
            else _summary_text(
                result,
                arguments.application_case_ids,
                arguments.application_workloads,
            )
        )
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
