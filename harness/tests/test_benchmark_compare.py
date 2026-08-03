from __future__ import annotations

import json
from pathlib import Path

import pytest

from rv32im_harness import benchmark_compare
from rv32im_harness.benchmark import BenchmarkFailure
from rv32im_harness.benchmark_compare import (
    _comparisons,
    _vm_mapping,
    main,
    run_comparison,
)


def _run_result(median: int, samples: list[int]) -> dict[str, object]:
    return {
        "schema_version": 1,
        "manifest_sha256": "a" * 64,
        "interface": "serve",
        "warmups": 2,
        "repetitions": len(samples),
        "timeout_seconds": 10.0,
        "cases": [
            {
                "id": "tiny",
                "workload": "tiny",
                "retired_instructions": 123,
                "samples_ns": samples,
                "median_ns": median,
            }
        ],
    }


def _native_result(median: int, samples: list[int]) -> dict[str, object]:
    result = _run_result(median, samples)
    result["interface"] = "native"
    del result["cases"][0]["retired_instructions"]
    return result


def _multi_case_result(
    medians: dict[str, int], *, interface: str = "serve"
) -> dict[str, object]:
    cases = []
    for case_id, median in medians.items():
        case = {
            "id": case_id,
            "workload": case_id,
            "samples_ns": [median],
            "median_ns": median,
        }
        if interface == "serve":
            case["retired_instructions"] = 123
        cases.append(case)
    return {
        "schema_version": 1,
        "manifest_sha256": "a" * 64,
        "interface": interface,
        "warmups": 2,
        "repetitions": 1,
        "timeout_seconds": 10.0,
        "cases": cases,
    }


def _horizon_result() -> dict[str, object]:
    vm4 = {
        "app-a-10x": 400,
        "app-b-10x": 900,
        "app-a-100x": 4_000,
        "app-b-100x": 9_000,
    }
    vm5 = {
        "app-a-10x": 100,
        "app-b-10x": 100,
        "app-a-100x": 2_000,
        "app-b-100x": 1_000,
    }
    native = {
        "app-a-10x": 25,
        "app-b-10x": 100,
        "app-a-100x": 1_000,
        "app-b-100x": 1_000,
    }
    return {
        "schema_version": 1,
        "baseline": "vm4",
        "implementations": {
            "vm4": {"interface": "serve", "path": "vm4"},
            "vm5": {"interface": "serve", "path": "vm5"},
            "native": {"interface": "native", "path": "/native"},
        },
        "runs": {
            "vm4": _multi_case_result(vm4),
            "vm5": _multi_case_result(vm5),
            "native": _multi_case_result(native, interface="native"),
        },
        "comparisons": [],
    }


def test_run_comparison_measures_vms_and_native_reference(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    baseline_result = _run_result(300, [290, 300, 310])
    fast_result = _run_result(100, [90, 100, 110])
    other_result = _run_result(150, [140, 150, 160])
    native_result = _native_result(50, [40, 50, 60])
    vm_calls = []
    native_calls = []
    results = iter((baseline_result, fast_result, other_result))

    def fake_run(*args: object, **kwargs: object) -> dict[str, object]:
        vm_calls.append((args, kwargs))
        return next(results)

    def fake_native(*args: object, **kwargs: object) -> dict[str, object]:
        native_calls.append((args, kwargs))
        return native_result

    monkeypatch.setattr(benchmark_compare, "run_benchmarks", fake_run)
    monkeypatch.setattr(benchmark_compare, "run_native_benchmarks", fake_native)
    result = run_comparison(
        {"base": "vm0", "fast": "vm1", "other": "vm2"},
        "base",
        "manifest",
        warmups=2,
        repetitions=3,
        timeout=10,
        case_ids=["tiny"],
        native=("native", "/native"),
    )

    assert vm_calls == [
        (
            (executable, "manifest"),
            {
                "warmups": 2,
                "repetitions": 3,
                "timeout": 10,
                "case_ids": ("tiny",),
            },
        )
        for executable in ("vm0", "vm1", "vm2")
    ]
    assert native_calls == [
        (
            ("/native", "manifest"),
            {
                "warmups": 2,
                "repetitions": 3,
                "timeout": 10,
                "case_ids": ("tiny",),
            },
        )
    ]
    assert result["baseline"] == "base"
    assert result["schema_version"] == 1
    assert result["implementations"] == {
        "base": {"interface": "serve", "path": "vm0"},
        "fast": {"interface": "serve", "path": "vm1"},
        "other": {"interface": "serve", "path": "vm2"},
        "native": {"interface": "native", "path": "/native"},
    }
    assert result["runs"] == {
        "base": baseline_result,
        "fast": fast_result,
        "other": other_result,
        "native": native_result,
    }
    assert result["comparisons"] == [
        {
            "implementation": "fast",
            "id": "tiny",
            "workload": "tiny",
            "retired_instructions": 123,
            "baseline_median_ns": 300,
            "implementation_median_ns": 100,
            "speedup": 3.0,
        },
        {
            "implementation": "other",
            "id": "tiny",
            "workload": "tiny",
            "retired_instructions": 123,
            "baseline_median_ns": 300,
            "implementation_median_ns": 150,
            "speedup": 2.0,
        },
        {
            "implementation": "native",
            "id": "tiny",
            "workload": "tiny",
            "retired_instructions": 123,
            "baseline_median_ns": 300,
            "implementation_median_ns": 50,
            "speedup": 6.0,
        },
    ]


@pytest.mark.parametrize(
    ("mutation", "message"),
    [
        (lambda result: result.update(warmups=1), "warmups"),
        (lambda result: result["cases"][0].update(id="other"), "case 0 id"),
        (
            lambda result: result["cases"][0].update(retired_instructions=124),
            "retired_instructions",
        ),
        (lambda result: result["cases"][0].update(median_ns=0), "median_ns"),
    ],
)
def test_comparison_rejects_incompatible_results(
    mutation,
    message: str,
) -> None:
    baseline_result = _run_result(300, [300])
    candidate_result = _run_result(100, [100])
    mutation(candidate_result)

    with pytest.raises(BenchmarkFailure, match=message):
        _comparisons(baseline_result, "candidate", candidate_result)


def test_comparison_rejects_invalid_vm_sets() -> None:
    with pytest.raises(BenchmarkFailure, match="at least two"):
        run_comparison({"only": "vm0"}, "only")
    with pytest.raises(BenchmarkFailure, match="baseline VM is not defined"):
        run_comparison({"one": "vm0", "two": "vm1"}, "missing")
    with pytest.raises(BenchmarkFailure, match="duplicate VM label"):
        _vm_mapping([("same", "vm0"), ("same", "vm1")])
    with pytest.raises(BenchmarkFailure, match="duplicate implementation label"):
        run_comparison(
            {"one": "vm0", "two": "vm1"},
            "one",
            native=("two", "/native"),
        )


def test_application_summary_uses_native_normalized_geometric_means() -> None:
    result = {
        "schema_version": 1,
        "baseline": "base",
        "implementations": {
            "base": {"interface": "serve", "path": "vm0"},
            "fast": {"interface": "serve", "path": "vm1"},
            "native": {"interface": "native", "path": "/native"},
        },
        "runs": {
            "base": _multi_case_result({"tiny": 300, "app-a": 400, "app-b": 900}),
            "fast": _multi_case_result({"tiny": 100, "app-a": 100, "app-b": 100}),
            "native": _multi_case_result(
                {"tiny": 50, "app-a": 25, "app-b": 100}, interface="native"
            ),
        },
        "comparisons": [],
    }

    summary = benchmark_compare._application_summary_text(result, ("app-a", "app-b"))

    assert summary is not None
    assert "geometric mean across 2 application workloads" in summary
    assert "excluded cases: tiny" in summary
    assert "speedup vs base" in summary
    assert "native performance" in summary
    parsed_rows = [
        fields
        for line in summary.splitlines()
        if (fields := line.split()) and fields[0] in {"base", "fast", "native"}
    ]
    assert [fields[0] for fields in parsed_rows] == ["base", "fast", "native"]
    rows = {fields[0]: fields[1:] for fields in parsed_rows}
    assert rows == {
        "base": ["1.000x", "8.3333%", "12.000x"],
        "fast": ["6.000x", "50.0000%", "2.000x"],
        "native": ["12.000x", "100.0000%", "1.000x"],
    }


def test_application_summary_skips_runs_without_selected_application_cases() -> None:
    result = {
        "schema_version": 1,
        "baseline": "base",
        "implementations": {
            "base": {"interface": "serve", "path": "vm0"},
            "native": {"interface": "native", "path": "/native"},
        },
        "runs": {
            "base": _multi_case_result({"tiny": 300}),
            "native": _multi_case_result({"tiny": 50}, interface="native"),
        },
        "comparisons": [],
    }

    assert benchmark_compare._application_summary_text(result, ("sha256",)) is None


def test_horizon_summary_groups_cases_and_compares_with_native() -> None:
    summary = benchmark_compare._horizon_summary_text(_horizon_result())

    assert "geometric mean across 2 application workloads per horizon" in summary
    assert "10x      vm5" in summary
    assert "6.000x" in summary
    assert "4.243x" in summary
    assert "50.0000%" in summary
    assert "70.7107%" in summary


def test_horizon_summary_rejects_different_case_cohorts() -> None:
    result = _horizon_result()
    for run in result["runs"].values():
        run["cases"][2]["id"] = "app-c-100x"

    with pytest.raises(BenchmarkFailure, match="different benchmark cases"):
        benchmark_compare._horizon_summary_text(result)


def test_main_selects_horizon_report(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    output = tmp_path / "comparison.json"
    monkeypatch.setattr(
        benchmark_compare,
        "run_comparison",
        lambda *args, **kwargs: _horizon_result(),
    )

    assert (
        main(
            [
                "manifest",
                "--vm",
                "vm4=vm4",
                "--vm",
                "vm5=vm5",
                "--native",
                "native=/native",
                "--baseline",
                "vm4",
                "--horizon-report",
                "--output",
                str(output),
            ]
        )
        == 0
    )
    assert "4.243x" in capsys.readouterr().out


def test_main_writes_raw_json_and_prints_summary(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    output = tmp_path / "nested/comparison.json"
    result = {
        "schema_version": 1,
        "baseline": "base",
        "implementations": {
            "base": {"interface": "serve", "path": "vm0"},
            "fast": {"interface": "serve", "path": "vm1"},
        },
        "runs": {
            "base": _run_result(300, [290, 300, 310]),
            "fast": _run_result(100, [90, 100, 110]),
        },
        "comparisons": [
            {
                "implementation": "fast",
                "id": "tiny",
                "workload": "tiny",
                "retired_instructions": 123,
                "baseline_median_ns": 300,
                "implementation_median_ns": 100,
                "speedup": 3.0,
            }
        ],
    }
    calls = []

    def fake_comparison(*args: object, **kwargs: object) -> dict[str, object]:
        calls.append((args, kwargs))
        return result

    monkeypatch.setattr(benchmark_compare, "run_comparison", fake_comparison)

    assert (
        main(
            [
                "manifest",
                "--vm",
                "base=vm0",
                "--vm",
                "fast=vm1",
                "--baseline",
                "base",
                "--native",
                "native=/native",
                "--warmups",
                "0",
                "--repetitions",
                "3",
                "--case",
                "tiny",
                "--output",
                str(output),
            ]
        )
        == 0
    )
    assert json.loads(output.read_text()) == result
    assert '"samples_ns": [' in output.read_text()
    stdout = capsys.readouterr().out
    assert "base median (ns)" in stdout
    assert "implementation median (ns)" in stdout
    assert "fast" in stdout
    assert "3.000x" in stdout
    assert str(output) in stdout
    assert calls == [
        (
            ({"base": "vm0", "fast": "vm1"}, "base", "manifest"),
            {
                "warmups": 0,
                "repetitions": 3,
                "timeout": 10.0,
                "case_ids": ["tiny"],
                "native": ("native", "/native"),
            },
        )
    ]
