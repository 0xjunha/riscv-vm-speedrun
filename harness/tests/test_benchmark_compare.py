from __future__ import annotations

import json
from pathlib import Path

import pytest

from rv32im_harness import benchmark_compare
from rv32im_harness.benchmark import BenchmarkCase, BenchmarkFailure, BenchmarkSuite
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
    medians: dict[str, int],
    *,
    workloads: dict[str, str] | None = None,
    interface: str = "serve",
) -> dict[str, object]:
    cases = []
    for case_id, median in medians.items():
        case = {
            "id": case_id,
            "workload": workloads[case_id] if workloads is not None else case_id,
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


def _loaded_suite(cases: tuple[tuple[str, str], ...]) -> BenchmarkSuite:
    return BenchmarkSuite(
        "a" * 64,
        tuple(
            BenchmarkCase(
                case_id,
                workload,
                "application",
                b"elf",
                b"input",
                b"expected",
                0,
                100,
                16,
            )
            for case_id, workload in cases
        ),
    )


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
    load_calls = []
    results = iter((baseline_result, fast_result, other_result))

    def fake_load(manifest: object) -> BenchmarkSuite:
        load_calls.append(manifest)
        return _loaded_suite((("tiny", "tiny"),))

    def fake_run(
        executable: str, suite: BenchmarkSuite, **options: object
    ) -> dict[str, object]:
        vm_calls.append((executable, suite, options))
        return next(results)

    def fake_native(
        directory: str, suite: BenchmarkSuite, **options: object
    ) -> dict[str, object]:
        native_calls.append((directory, suite, options))
        return native_result

    monkeypatch.setattr(benchmark_compare, "load_benchmark_suite", fake_load)
    monkeypatch.setattr(benchmark_compare, "run_benchmark_suite", fake_run)
    monkeypatch.setattr(benchmark_compare, "run_native_benchmark_suite", fake_native)
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

    assert load_calls == ["manifest"]
    expected_options = {"warmups": 2, "repetitions": 3, "timeout": 10}
    assert [
        (executable, suite.cases[0].case_id, options)
        for executable, suite, options in vm_calls
    ] == [
        (executable, "tiny", expected_options) for executable in ("vm0", "vm1", "vm2")
    ]
    assert [
        (directory, suite.cases[0].case_id, options)
        for directory, suite, options in native_calls
    ] == [("/native", "tiny", expected_options)]
    assert result["baseline"] == "base"
    assert result["schema_version"] == 1
    assert result["schedule"] == {
        "strategy": "case_interleaved_rotating_participants",
        "case_order": ["tiny"],
        "initial_participant_order": ["base", "fast", "other", "native"],
        "rotation": "left_by_case_index",
    }
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


def test_run_comparison_interleaves_cases_and_rotates_vm_order(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    manifest_cases = (
        ("case-a", "decode"),
        ("case-b", "crypto"),
        ("case-c", "storage"),
        ("case-d", "decode"),
    )
    workloads = dict(manifest_cases)
    medians = {
        "vm0": {
            case_id: 300 + index for index, (case_id, _) in enumerate(manifest_cases)
        },
        "vm1": {
            case_id: 200 + index for index, (case_id, _) in enumerate(manifest_cases)
        },
        "vm2": {
            case_id: 100 + index for index, (case_id, _) in enumerate(manifest_cases)
        },
        "/native": {
            case_id: 50 + index for index, (case_id, _) in enumerate(manifest_cases)
        },
    }
    call_order = []

    def fake_run(
        executable: str, suite: BenchmarkSuite, **options: object
    ) -> dict[str, object]:
        case_id = suite.cases[0].case_id
        call_order.append((case_id, executable))
        return _multi_case_result(
            {case_id: medians[executable][case_id]},
            workloads={case_id: workloads[case_id]},
        )

    def fake_native(
        directory: str, suite: BenchmarkSuite, **options: object
    ) -> dict[str, object]:
        case_id = suite.cases[0].case_id
        call_order.append((case_id, "native"))
        return _multi_case_result(
            {case_id: medians[directory][case_id]},
            workloads={case_id: workloads[case_id]},
            interface="native",
        )

    monkeypatch.setattr(
        benchmark_compare,
        "load_benchmark_suite",
        lambda *args: _loaded_suite(manifest_cases),
    )
    monkeypatch.setattr(benchmark_compare, "run_benchmark_suite", fake_run)
    monkeypatch.setattr(benchmark_compare, "run_native_benchmark_suite", fake_native)

    result = run_comparison(
        {"base": "vm0", "fast": "vm1", "other": "vm2"},
        "base",
        "manifest",
        repetitions=1,
        native=("native", "/native"),
    )

    assert call_order == [
        ("case-a", "vm0"),
        ("case-a", "vm1"),
        ("case-a", "vm2"),
        ("case-a", "native"),
        ("case-b", "vm1"),
        ("case-b", "vm2"),
        ("case-b", "native"),
        ("case-b", "vm0"),
        ("case-c", "vm2"),
        ("case-c", "native"),
        ("case-c", "vm0"),
        ("case-c", "vm1"),
        ("case-d", "native"),
        ("case-d", "vm0"),
        ("case-d", "vm1"),
        ("case-d", "vm2"),
    ]
    assert result["schedule"] == {
        "strategy": "case_interleaved_rotating_participants",
        "case_order": ["case-a", "case-b", "case-c", "case-d"],
        "initial_participant_order": ["base", "fast", "other", "native"],
        "rotation": "left_by_case_index",
    }
    assert list(result["runs"]) == ["base", "fast", "other", "native"]
    for label in ("base", "fast", "other", "native"):
        assert [case["id"] for case in result["runs"][label]["cases"]] == [
            "case-a",
            "case-b",
            "case-c",
            "case-d",
        ]
    assert result["runs"]["base"] == _multi_case_result(
        medians["vm0"], workloads=workloads
    )
    assert result["runs"]["native"] == _multi_case_result(
        medians["/native"], workloads=workloads, interface="native"
    )


def test_run_comparison_reports_interleaved_vm_failure_with_label(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    manifest_cases = (("case-a", "work"), ("case-b", "work"))
    calls = []

    def fake_run(
        executable: str, suite: BenchmarkSuite, **options: object
    ) -> dict[str, object]:
        case_id = suite.cases[0].case_id
        calls.append((case_id, executable))
        if executable == "vm1" and case_id == "case-b":
            raise BenchmarkFailure("case-b failed")
        return _multi_case_result({case_id: 100}, workloads={case_id: "work"})

    monkeypatch.setattr(
        benchmark_compare,
        "load_benchmark_suite",
        lambda *args: _loaded_suite(manifest_cases),
    )
    monkeypatch.setattr(benchmark_compare, "run_benchmark_suite", fake_run)

    with pytest.raises(BenchmarkFailure, match="fast: case-b failed"):
        run_comparison(
            {"base": "vm0", "fast": "vm1", "other": "vm2"},
            "base",
            "manifest",
            repetitions=1,
        )

    assert calls == [
        ("case-a", "vm0"),
        ("case-a", "vm1"),
        ("case-a", "vm2"),
        ("case-b", "vm1"),
    ]


def test_run_comparison_rejects_metadata_drift_between_case_chunks(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    manifest_cases = (("case-a", "work"), ("case-b", "work"))

    def fake_run(
        executable: str, suite: BenchmarkSuite, **options: object
    ) -> dict[str, object]:
        case_id = suite.cases[0].case_id
        result = _multi_case_result({case_id: 100}, workloads={case_id: "work"})
        if executable == "vm0" and case_id == "case-b":
            result["manifest_sha256"] = "b" * 64
        return result

    monkeypatch.setattr(
        benchmark_compare,
        "load_benchmark_suite",
        lambda *args: _loaded_suite(manifest_cases),
    )
    monkeypatch.setattr(benchmark_compare, "run_benchmark_suite", fake_run)

    with pytest.raises(
        BenchmarkFailure, match="base: case-b run disagrees on manifest_sha256"
    ):
        run_comparison(
            {"base": "vm0", "fast": "vm1"},
            "base",
            "manifest",
            repetitions=1,
        )


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


def test_application_summary_rejects_unknown_application_cases() -> None:
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

    with pytest.raises(BenchmarkFailure, match="unknown application case IDs: sha256"):
        benchmark_compare._application_summary_text(result, ("sha256",))


def test_application_summary_selects_workloads_and_weights_each_equally() -> None:
    workloads = {
        "diagnostic": "diagnostic",
        "decode-low": "decode",
        "decode-high": "decode",
        "crypto-vector": "crypto",
    }
    result = {
        "schema_version": 1,
        "baseline": "base",
        "implementations": {
            "base": {"interface": "serve", "path": "vm0"},
            "fast": {"interface": "serve", "path": "vm1"},
            "native": {"interface": "native", "path": "/native"},
        },
        "runs": {
            "base": _multi_case_result(
                {
                    "diagnostic": 100,
                    "decode-low": 400,
                    "decode-high": 400,
                    "crypto-vector": 100,
                },
                workloads=workloads,
            ),
            "fast": _multi_case_result(
                {
                    "diagnostic": 100,
                    "decode-low": 100,
                    "decode-high": 100,
                    "crypto-vector": 100,
                },
                workloads=workloads,
            ),
            "native": _multi_case_result(
                {
                    "diagnostic": 100,
                    "decode-low": 25,
                    "decode-high": 25,
                    "crypto-vector": 25,
                },
                workloads=workloads,
                interface="native",
            ),
        },
        "comparisons": [],
    }

    summary = benchmark_compare._application_summary_text(
        result, application_workloads=("decode", "crypto")
    )

    assert summary is not None
    assert "geometric mean across 2 application workloads" in summary
    assert "geometric means within each workload (3 cases total)" in summary
    assert "excluded cases: diagnostic" in summary
    fast_row = next(
        line.split() for line in summary.splitlines() if line.startswith("fast ")
    )
    assert fast_row == ["fast", "2.000x", "25.0000%", "4.000x"]

    legacy_summary = benchmark_compare._application_summary_text(
        result, ("decode-low", "decode-high", "crypto-vector")
    )
    assert legacy_summary is not None
    legacy_fast_row = next(
        line.split() for line in legacy_summary.splitlines() if line.startswith("fast ")
    )
    assert legacy_fast_row == fast_row


@pytest.mark.parametrize(
    ("case_ids", "workloads", "message"),
    [
        (("a", "a"), (), "application case ID selection contains duplicates: a"),
        ((), ("work", "work"), "application workload selection contains duplicates"),
        (("a",), ("work",), "cannot mix case and workload selections"),
        ((), ("missing",), "unknown application workloads: missing"),
    ],
)
def test_application_summary_rejects_invalid_selections(
    case_ids: tuple[str, ...], workloads: tuple[str, ...], message: str
) -> None:
    result = {
        "schema_version": 1,
        "baseline": "base",
        "implementations": {
            "base": {"interface": "serve", "path": "vm0"},
            "native": {"interface": "native", "path": "/native"},
        },
        "runs": {
            "base": _multi_case_result({"a": 100}),
            "native": _multi_case_result({"a": 50}, interface="native"),
        },
        "comparisons": [],
    }

    with pytest.raises(BenchmarkFailure, match=message):
        benchmark_compare._application_summary_text(result, case_ids, workloads)


def test_application_summary_rejects_mismatched_workload_metadata() -> None:
    result = {
        "schema_version": 1,
        "baseline": "base",
        "implementations": {
            "base": {"interface": "serve", "path": "vm0"},
            "native": {"interface": "native", "path": "/native"},
        },
        "runs": {
            "base": _multi_case_result({"a": 100}),
            "native": _multi_case_result(
                {"a": 50}, workloads={"a": "different"}, interface="native"
            ),
        },
        "comparisons": [],
    }

    with pytest.raises(BenchmarkFailure, match="different workload metadata"):
        benchmark_compare._application_summary_text(
            result, application_workloads=("a",)
        )


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


def test_main_selects_all_cases_for_application_workload(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    output = tmp_path / "comparison.json"
    workloads = {"decode-low": "decode", "decode-high": "decode"}
    result = {
        "schema_version": 1,
        "baseline": "base",
        "implementations": {
            "base": {"interface": "serve", "path": "vm0"},
            "native": {"interface": "native", "path": "/native"},
        },
        "runs": {
            "base": _multi_case_result(
                {"decode-low": 100, "decode-high": 400}, workloads=workloads
            ),
            "native": _multi_case_result(
                {"decode-low": 50, "decode-high": 100},
                workloads=workloads,
                interface="native",
            ),
        },
        "comparisons": [
            {
                "implementation": "other",
                "id": "decode-low",
                "workload": "decode",
                "retired_instructions": 123,
                "baseline_median_ns": 100,
                "implementation_median_ns": 100,
                "speedup": 1.0,
            }
        ],
    }
    monkeypatch.setattr(
        benchmark_compare, "run_comparison", lambda *args, **kwargs: result
    )

    assert (
        main(
            [
                "manifest",
                "--vm",
                "base=vm0",
                "--vm",
                "other=vm1",
                "--native",
                "native=/native",
                "--baseline",
                "base",
                "--application-workload",
                "decode",
                "--output",
                str(output),
            ]
        )
        == 0
    )
    stdout = capsys.readouterr().out
    assert "geometric mean across 1 application workload" in stdout
    assert "geometric means within each workload (2 cases total)" in stdout


def test_main_rejects_mixed_application_selectors_before_running(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    def unexpected_run(*args: object, **kwargs: object) -> dict[str, object]:
        pytest.fail("run_comparison should not be called")

    monkeypatch.setattr(benchmark_compare, "run_comparison", unexpected_run)

    assert (
        main(
            [
                "manifest",
                "--vm",
                "base=vm0",
                "--vm",
                "other=vm1",
                "--baseline",
                "base",
                "--application-case",
                "decode-low",
                "--application-workload",
                "decode",
                "--output",
                str(tmp_path / "comparison.json"),
            ]
        )
        == 1
    )
    assert "--application-case conflicts with --application-workload" in (
        capsys.readouterr().err
    )


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
                "timeout": 30.0,
                "case_ids": ["tiny"],
                "native": ("native", "/native"),
            },
        )
    ]
