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


def test_run_comparison_measures_labeled_vms_and_preserves_raw_runs(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    baseline_result = _run_result(300, [290, 300, 310])
    fast_result = _run_result(100, [90, 100, 110])
    other_result = _run_result(150, [140, 150, 160])
    calls = []
    results = iter((baseline_result, fast_result, other_result))

    def fake_run(*args: object, **kwargs: object) -> dict[str, object]:
        calls.append((args, kwargs))
        return next(results)

    monkeypatch.setattr(benchmark_compare, "run_benchmarks", fake_run)
    result = run_comparison(
        {"base": "vm0", "fast": "vm1", "other": "vm2"},
        "base",
        "manifest",
        warmups=2,
        repetitions=3,
        timeout=10,
        case_ids=["tiny"],
    )

    assert calls == [
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
    assert result["baseline"] == "base"
    assert result["runs"] == {
        "base": baseline_result,
        "fast": fast_result,
        "other": other_result,
    }
    assert result["comparisons"] == [
        {
            "candidate": "fast",
            "id": "tiny",
            "workload": "tiny",
            "retired_instructions": 123,
            "baseline_median_ns": 300,
            "candidate_median_ns": 100,
            "speedup": 3.0,
        },
        {
            "candidate": "other",
            "id": "tiny",
            "workload": "tiny",
            "retired_instructions": 123,
            "baseline_median_ns": 300,
            "candidate_median_ns": 150,
            "speedup": 2.0,
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


def test_main_writes_raw_json_and_prints_summary(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    output = tmp_path / "nested/comparison.json"
    result = {
        "schema_version": 1,
        "baseline": "base",
        "executables": {"base": "vm0", "fast": "vm1"},
        "runs": {
            "base": _run_result(300, [290, 300, 310]),
            "fast": _run_result(100, [90, 100, 110]),
        },
        "comparisons": [
            {
                "candidate": "fast",
                "id": "tiny",
                "workload": "tiny",
                "retired_instructions": 123,
                "baseline_median_ns": 300,
                "candidate_median_ns": 100,
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
            },
        )
    ]
