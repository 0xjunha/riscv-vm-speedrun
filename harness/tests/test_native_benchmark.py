from __future__ import annotations

import hashlib
import json
from pathlib import Path

import pytest

from rv32im_harness.benchmark import BenchmarkFailure
from rv32im_harness.native_benchmark import run_native_benchmarks


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


def _manifest(temporary: Path, expected: bytes = b"input") -> Path:
    root = temporary / "artifacts"
    root.mkdir()
    record: dict[str, object] = {
        "id": "tiny",
        "workload": "tiny",
        "expected_exit_code": 0,
        "instruction_limit": 100,
        "output_limit": 64,
    }
    _artifact(root, record, "elf", "elf/tiny.elf", b"elf")
    _artifact(root, record, "input", "input/tiny.bin", b"input")
    _artifact(root, record, "expected_output", "expected/tiny.bin", expected)
    path = root / "manifest.json"
    path.write_text(json.dumps({"schema_version": 1, "cases": [record]}))
    return path


def _native_executable(temporary: Path) -> Path:
    directory = temporary / "native"
    directory.mkdir()
    executable = directory / "tiny"
    executable.write_text(
        """#!/usr/bin/env python3
import json
import sys

warmups, repetitions = map(int, sys.argv[1:])
data = sys.stdin.buffer.read()
assert warmups >= 0
print(json.dumps({
    "schema_version": 1,
    "output_hex": data.hex(),
    "samples_ns": list(range(10, 10 + repetitions)),
}, separators=(",", ":")))
"""
    )
    executable.chmod(0o755)
    return directory


def test_native_benchmark_runs_workload_executable_and_preserves_samples(
    tmp_path: Path,
) -> None:
    manifest = _manifest(tmp_path)
    directory = _native_executable(tmp_path)

    result = run_native_benchmarks(
        directory,
        manifest,
        warmups=1,
        repetitions=3,
        timeout=2,
    )

    assert result == {
        "schema_version": 1,
        "manifest_sha256": hashlib.sha256(manifest.read_bytes()).hexdigest(),
        "interface": "native",
        "warmups": 1,
        "repetitions": 3,
        "timeout_seconds": 2.0,
        "cases": [
            {
                "id": "tiny",
                "workload": "tiny",
                "samples_ns": [10, 11, 12],
                "median_ns": 11,
            }
        ],
    }


def test_native_benchmark_rejects_incorrect_output(tmp_path: Path) -> None:
    manifest = _manifest(tmp_path, expected=b"wrong")
    directory = _native_executable(tmp_path)

    with pytest.raises(BenchmarkFailure, match="native output differs"):
        run_native_benchmarks(directory, manifest, warmups=0, repetitions=1)


@pytest.mark.parametrize(
    ("warmups", "repetitions", "message"),
    [
        (-1, 1, "warmups"),
        (0, 0, "repetitions"),
        (0, 1, "native executable directory"),
    ],
)
def test_native_benchmark_rejects_invalid_configuration(
    tmp_path: Path,
    warmups: int,
    repetitions: int,
    message: str,
) -> None:
    directory = _native_executable(tmp_path)
    if message == "native executable directory":
        directory = tmp_path / "missing"

    with pytest.raises(BenchmarkFailure, match=message):
        run_native_benchmarks(
            directory,
            _manifest(tmp_path),
            warmups=warmups,
            repetitions=repetitions,
        )
