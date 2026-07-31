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
