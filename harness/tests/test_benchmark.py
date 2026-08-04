from __future__ import annotations

import hashlib
import json
import os
import statistics
from pathlib import Path
from typing import Self

import pytest

from rv32im_harness import benchmark
from rv32im_harness.benchmark import (
    BenchmarkFailure,
    _load_manifest,
    _read_artifact,
    _require_outcome,
    _select_cases,
    load_benchmark_suite,
    main,
    run_benchmark_suite,
    run_benchmarks,
)
from rv32im_harness.vm_interface import MAX_INPUT_SIZE, RunOutcome, RunResult, Trap

STUB_VM = Path(__file__).with_name("stub_vm.py")


def _artifact(
    root: Path,
    record: dict[str, object],
    key: str,
    name: str,
    data: bytes,
) -> None:
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    record[key] = name
    record[f"{key}_sha256"] = hashlib.sha256(data).hexdigest()
    record[f"{key}_size"] = len(data)


def _manifest(
    temporary: Path,
    case_ids: tuple[str, ...] = ("tiny",),
    *,
    expected_output: bytes | None = None,
) -> Path:
    root = temporary / "benchmarks/artifacts"
    root.mkdir(parents=True)
    records = []
    for case_id in case_ids:
        input_data = case_id.encode()
        record: dict[str, object] = {
            "id": case_id,
            "workload": case_id,
            "category": "diagnostic",
            "regime": "smoke",
            "expected_exit_code": 0,
            "instruction_limit": 100,
            "output_limit": 64,
        }
        _artifact(root, record, "elf", f"elf/{case_id}.elf", b"ELF-" + input_data)
        _artifact(root, record, "input", f"input/{case_id}.bin", input_data)
        _artifact(
            root,
            record,
            "expected_output",
            f"expected/{case_id}.bin",
            input_data if expected_output is None else expected_output,
        )
        records.append(record)
    path = root / "manifest.json"
    path.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "builder": {"platform": "test"},
                "project_inputs": {},
                "cases": records,
            },
            sort_keys=True,
        )
    )
    return path


def _outcome(
    output: bytes,
    *,
    retired: int = 3,
    status: str = "exit",
    exit_code: int | None = 0,
) -> RunOutcome:
    trap = Trap("IllegalInstruction", 0x10000, 0) if status == "trap" else None
    return RunOutcome(
        RunResult(1, status, exit_code, trap, None, retired, len(output)),
        output,
    )


def test_run_benchmarks_measures_only_persistent_runs(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    manifest = _manifest(tmp_path)
    log = tmp_path / "requests"
    monkeypatch.setenv("STUB_VM_LOG", str(log))

    result = run_benchmarks(
        STUB_VM,
        manifest,
        warmups=1,
        repetitions=3,
    )

    assert result == {
        "schema_version": 1,
        "manifest_sha256": hashlib.sha256(manifest.read_bytes()).hexdigest(),
        "interface": "serve",
        "warmups": 1,
        "repetitions": 3,
        "timeout_seconds": 30.0,
        "cases": [
            {
                "id": "tiny",
                "workload": "tiny",
                "retired_instructions": 3,
                "samples_ns": result["cases"][0]["samples_ns"],
                "median_ns": result["cases"][0]["median_ns"],
            }
        ],
    }
    samples = result["cases"][0]["samples_ns"]
    assert all(type(sample) is int and sample > 0 for sample in samples)
    assert result["cases"][0]["median_ns"] == statistics.median(samples)
    assert "score" not in json.dumps(result)
    assert log.read_text().splitlines() == [
        "load",
        "run",  # correctness
        "run",  # warmup
        "run",
        "run",
        "run",
        "unload",
        "shutdown",
    ]


def test_correctness_failure_precedes_timing(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    manifest = _manifest(tmp_path, expected_output=b"wrong")

    def unexpected_clock() -> int:
        raise AssertionError("clock called before correctness passed")

    monkeypatch.setattr(benchmark.time, "perf_counter_ns", unexpected_clock)

    with pytest.raises(BenchmarkFailure, match="correctness run: output differs"):
        run_benchmarks(STUB_VM, manifest, warmups=0, repetitions=1)


@pytest.mark.parametrize(
    ("outcome", "message"),
    [
        (_outcome(b"tiny", status="trap", exit_code=None), "expected exit"),
        (_outcome(b"tiny", exit_code=7), "exit code 0"),
        (_outcome(b"other"), "output differs"),
    ],
)
def test_outcome_validation_rejects_incorrect_results(
    tmp_path: Path,
    outcome: RunOutcome,
    message: str,
) -> None:
    case = _load_manifest(_manifest(tmp_path)).cases[0]

    with pytest.raises(BenchmarkFailure, match=message):
        _require_outcome(case, "correctness run", outcome)


def test_timed_result_is_checked_after_stopping_clock(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    manifest = _manifest(tmp_path)
    outcomes = iter((_outcome(b"tiny"), _outcome(b"tiny", retired=4)))
    clock_calls = []

    class FakeServer:
        def __init__(self, _executable: object, *, timeout: float) -> None:
            assert timeout == 2.5

        def __enter__(self) -> Self:
            return self

        def __exit__(self, *_arguments: object) -> None:
            pass

        def load(self, _elf: bytes) -> None:
            pass

        def run(self, *_arguments: object, **_keywords: object) -> RunOutcome:
            return next(outcomes)

        def unload(self) -> None:
            pass

    ticks = iter((100, 160))

    def clock() -> int:
        clock_calls.append(None)
        return next(ticks)

    monkeypatch.setattr(benchmark, "VmServer", FakeServer)
    monkeypatch.setattr(benchmark.time, "perf_counter_ns", clock)

    with pytest.raises(BenchmarkFailure, match="instruction count changed"):
        run_benchmarks(
            "unused",
            manifest,
            warmups=0,
            repetitions=1,
            timeout=2.5,
        )
    assert len(clock_calls) == 2


def test_run_benchmarks_rejects_nonpositive_elapsed_time(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(benchmark.time, "perf_counter_ns", lambda: 100)

    with pytest.raises(BenchmarkFailure, match="clock did not advance"):
        run_benchmarks(
            STUB_VM,
            _manifest(tmp_path),
            warmups=0,
            repetitions=1,
        )


def test_each_selected_case_uses_a_fresh_server(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    manifest = _manifest(tmp_path, ("tiny", "streaming"))
    log = tmp_path / "requests"
    monkeypatch.setenv("STUB_VM_LOG", str(log))

    result = run_benchmarks(
        STUB_VM,
        manifest,
        warmups=0,
        repetitions=1,
        case_ids=("streaming", "tiny"),
    )

    assert [case["id"] for case in result["cases"]] == ["streaming", "tiny"]
    assert log.read_text().splitlines() == [
        "load",
        "run",
        "run",
        "unload",
        "shutdown",
        "load",
        "run",
        "run",
        "unload",
        "shutdown",
    ]


def test_loaded_suite_selection_reuses_immutable_artifacts(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    manifest = _manifest(tmp_path, ("tiny", "streaming"))
    suite = load_benchmark_suite(manifest)
    selected = suite.select(("streaming",))
    log = tmp_path / "requests"
    monkeypatch.setenv("STUB_VM_LOG", str(log))

    # Once loaded, a selection is independent of later on-disk changes.
    (manifest.parent / "input/streaming.bin").write_bytes(b"changed")
    result = run_benchmark_suite(
        STUB_VM,
        selected,
        warmups=0,
        repetitions=1,
    )

    assert suite.cases != selected.cases
    assert [case.case_id for case in suite.cases] == ["tiny", "streaming"]
    assert [case.category for case in suite.cases] == ["diagnostic", "diagnostic"]
    assert [case.case_id for case in selected.cases] == ["streaming"]
    assert [case["id"] for case in result["cases"]] == ["streaming"]
    assert result["manifest_sha256"] == suite.sha256


@pytest.mark.parametrize(
    ("case_ids", "message"),
    [
        ((), "empty"),
        (("tiny", "tiny"), "duplicate"),
        (("missing",), "unknown"),
    ],
)
def test_case_filter_rejects_invalid_selection(
    tmp_path: Path,
    case_ids: tuple[str, ...],
    message: str,
) -> None:
    cases = _load_manifest(_manifest(tmp_path)).cases

    with pytest.raises(BenchmarkFailure, match=message):
        _select_cases(cases, case_ids)


def test_manifest_verifies_artifact_hash_and_size(tmp_path: Path) -> None:
    manifest = _manifest(tmp_path)
    root = manifest.parent

    (root / "input/tiny.bin").write_bytes(b"TINY")
    with pytest.raises(BenchmarkFailure, match="input hash"):
        _load_manifest(manifest)

    document = json.loads(manifest.read_text())
    document["cases"][0]["input_size"] = 3
    manifest.write_text(json.dumps(document))
    with pytest.raises(BenchmarkFailure, match="input size"):
        _load_manifest(manifest)


def test_schema_v1_manifest_defaults_missing_category_to_diagnostic(
    tmp_path: Path,
) -> None:
    manifest = _manifest(tmp_path)
    document = json.loads(manifest.read_text())
    del document["cases"][0]["category"]
    manifest.write_text(json.dumps(document))

    suite = load_benchmark_suite(manifest)

    assert suite.cases[0].category == "diagnostic"


@pytest.mark.parametrize("category", [None, "other", 1])
def test_manifest_rejects_invalid_present_category(
    tmp_path: Path, category: object
) -> None:
    manifest = _manifest(tmp_path)
    document = json.loads(manifest.read_text())
    document["cases"][0]["category"] = category
    manifest.write_text(json.dumps(document))

    with pytest.raises(BenchmarkFailure, match="category is invalid"):
        load_benchmark_suite(manifest)


def test_manifest_rejects_artifact_size_metadata_above_its_limit(
    tmp_path: Path,
) -> None:
    manifest = _manifest(tmp_path)
    document = json.loads(manifest.read_text())
    document["cases"][0]["input_size"] = MAX_INPUT_SIZE + 1
    manifest.write_text(json.dumps(document))

    with pytest.raises(BenchmarkFailure, match="input size exceeds"):
        _load_manifest(manifest)


def test_manifest_rejects_on_disk_artifact_above_its_limit(tmp_path: Path) -> None:
    manifest = _manifest(tmp_path)
    (manifest.parent / "expected/tiny.bin").write_bytes(bytes(65))

    with pytest.raises(BenchmarkFailure, match="expected_output size exceeds"):
        _load_manifest(manifest)


@pytest.mark.skipif(not hasattr(os, "mkfifo"), reason="mkfifo is unavailable")
def test_artifact_reader_rejects_fifo_before_opening(tmp_path: Path) -> None:
    fifo = tmp_path / "input.fifo"
    os.mkfifo(fifo)
    record = {
        "input": fifo.name,
        "input_sha256": hashlib.sha256(b"").hexdigest(),
        "input_size": 0,
    }

    with pytest.raises(BenchmarkFailure, match="not a regular file"):
        _read_artifact(tmp_path, record, "input", "case", MAX_INPUT_SIZE)


@pytest.mark.skipif(not hasattr(os, "symlink"), reason="symlinks are unavailable")
def test_artifact_reader_normalizes_symlink_loop_error(tmp_path: Path) -> None:
    first = tmp_path / "first"
    second = tmp_path / "second"
    first.symlink_to(second.name)
    second.symlink_to(first.name)
    record = {
        "input": first.name,
        "input_sha256": hashlib.sha256(b"").hexdigest(),
        "input_size": 0,
    }

    with pytest.raises(BenchmarkFailure, match="cannot read input"):
        _read_artifact(tmp_path, record, "input", "case", MAX_INPUT_SIZE)


def test_artifact_reader_normalizes_embedded_nul_error(tmp_path: Path) -> None:
    record = {
        "input": "bad\0path",
        "input_sha256": hashlib.sha256(b"").hexdigest(),
        "input_size": 0,
    }

    with pytest.raises(BenchmarkFailure, match="cannot read input"):
        _read_artifact(tmp_path, record, "input", "case", MAX_INPUT_SIZE)


def test_manifest_rejects_artifact_path_escape(tmp_path: Path) -> None:
    manifest = _manifest(tmp_path)
    document = json.loads(manifest.read_text())
    document["cases"][0]["input"] = "../outside.bin"
    manifest.write_text(json.dumps(document))

    with pytest.raises(BenchmarkFailure, match="leaves the artifact directory"):
        _load_manifest(manifest)


@pytest.mark.parametrize(
    ("warmups", "repetitions", "message"),
    [
        (-1, 1, "warmups"),
        (0, 0, "repetitions"),
        (True, 1, "warmups"),
    ],
)
def test_run_benchmarks_rejects_invalid_counts(
    warmups: int,
    repetitions: int,
    message: str,
) -> None:
    with pytest.raises(BenchmarkFailure, match=message):
        run_benchmarks(
            "unused",
            "unused",
            warmups=warmups,
            repetitions=repetitions,
        )


def test_main_writes_stable_json(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    output = tmp_path / "result.json"
    expected = {
        "schema_version": 1,
        "interface": "serve",
        "cases": [],
    }
    calls = []

    def fake_run(*arguments: object, **keywords: object) -> dict[str, object]:
        calls.append((arguments, keywords))
        return expected

    monkeypatch.setattr(benchmark, "run_benchmarks", fake_run)

    assert (
        main(
            [
                "vm",
                "manifest",
                "--warmups",
                "0",
                "--repetitions",
                "2",
                "--timeout",
                "3",
                "--case",
                "tiny",
                "--output",
                str(output),
            ]
        )
        == 0
    )
    assert output.read_text() == json.dumps(expected, indent=2, sort_keys=True) + "\n"
    assert calls == [
        (
            ("vm", "manifest"),
            {
                "warmups": 0,
                "repetitions": 2,
                "timeout": 3.0,
                "case_ids": ["tiny"],
            },
        )
    ]
