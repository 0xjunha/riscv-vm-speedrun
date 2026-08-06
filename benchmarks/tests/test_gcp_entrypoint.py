from __future__ import annotations

import os
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ENTRYPOINT = ROOT / "benchmarks/gcp/run-benchmark.sh"


def _entrypoint_arguments(tmp_path: Path, user_arguments: tuple[str, ...]) -> list[str]:
    entrypoint = tmp_path / "run-benchmark.sh"
    entrypoint.write_text(ENTRYPOINT.read_text(encoding="utf-8"), encoding="utf-8")
    entrypoint.chmod(0o755)

    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    log = tmp_path / "python-arguments.txt"
    python = fake_bin / "python3"
    python.write_text(
        """#!/bin/sh
set -eu
printf '%s\n' "$@" >"$ENTRYPOINT_LOG"
""",
        encoding="utf-8",
    )
    python.chmod(0o755)
    environment = {
        **os.environ,
        "ENTRYPOINT_LOG": str(log),
        "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
    }

    completed = subprocess.run(
        [str(entrypoint), *user_arguments],
        cwd=ROOT,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
    )

    assert completed.returncode == 0, completed.stderr
    return log.read_text(encoding="utf-8").splitlines()


def test_entrypoint_delegates_aggregate_policy_to_comparator(tmp_path: Path) -> None:
    arguments = _entrypoint_arguments(tmp_path, ("--warmups", "0"))

    assert arguments[:3] == [
        "-m",
        "rv32im_harness.benchmark_compare",
        "/opt/rv32im/benchmarks/manifest.json",
    ]
    assert arguments[-2:] == ["--warmups", "0"]


def test_entrypoint_selects_long_horizon_suite(tmp_path: Path) -> None:
    arguments = _entrypoint_arguments(tmp_path, ("--long", "--warmups", "0"))

    assert arguments[:3] == [
        "-m",
        "rv32im_harness.benchmark_compare",
        "/opt/rv32im/long-benchmarks/manifest.json",
    ]
    assert arguments[arguments.index("--vm") + 1] == "vm4=/opt/rv32im/vms/vm4/rv32vm"
    assert "vm5=/opt/rv32im/vms/vm5/rv32vm" in arguments
    assert "--horizon-report" in arguments
    assert "--long" not in arguments
    assert arguments[-2:] == ["--warmups", "0"]


def test_entrypoint_is_posix_shell_syntax() -> None:
    completed = subprocess.run(
        ["sh", "-n", str(ENTRYPOINT)],
        check=False,
        capture_output=True,
        text=True,
    )

    assert completed.returncode == 0, completed.stderr
