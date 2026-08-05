from __future__ import annotations

import json
import os
import shlex
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
ENTRYPOINT = ROOT / "benchmarks/gcp/run-benchmark.sh"


def _entrypoint_arguments(tmp_path: Path, user_arguments: tuple[str, ...]) -> list[str]:
    manifest = tmp_path / "manifest.json"
    manifest.write_text(
        json.dumps({"application_workloads": ["sha256", "qrcode"]}),
        encoding="utf-8",
    )
    entrypoint = tmp_path / "run-benchmark.sh"
    entrypoint.write_text(
        ENTRYPOINT.read_text(encoding="utf-8").replace(
            "manifest=/opt/rv32im/benchmarks/manifest.json",
            f"manifest={shlex.quote(str(manifest))}",
            1,
        ),
        encoding="utf-8",
    )
    entrypoint.chmod(0o755)

    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    log = tmp_path / "python-arguments.txt"
    python = fake_bin / "python3"
    python.write_text(
        f"""#!/bin/sh
set -eu
if [ "${{1:-}}" = - ]; then
    exec {shlex.quote(sys.executable)} "$@"
fi
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


def _option_values(arguments: list[str], option: str) -> list[str]:
    values = []
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument == option:
            values.append(arguments[index + 1])
            index += 2
        elif argument.startswith(f"{option}="):
            values.append(argument.split("=", 1)[1])
            index += 1
        else:
            index += 1
    return values


def test_entrypoint_adds_manifest_application_workloads_by_default(
    tmp_path: Path,
) -> None:
    arguments = _entrypoint_arguments(tmp_path, ("--warmups", "0"))

    assert _option_values(arguments, "--application-workload") == [
        "sha256",
        "qrcode",
    ]
    assert arguments[-2:] == ["--application-workload", "qrcode"]


@pytest.mark.parametrize(
    "selector",
    [
        ("--case", "tiny"),
        ("--case=tiny",),
        ("--application-case", "sha256"),
        ("--application-case=sha256",),
        ("--application-workload", "littlefs"),
        ("--application-workload=littlefs",),
    ],
)
def test_entrypoint_preserves_explicit_selection_without_auto_injection(
    tmp_path: Path, selector: tuple[str, ...]
) -> None:
    arguments = _entrypoint_arguments(tmp_path, selector)

    expected_workloads = ["littlefs"] if "application-workload" in selector[0] else []
    assert _option_values(arguments, "--application-workload") == expected_workloads
    assert arguments[-len(selector) :] == list(selector)


def test_entrypoint_is_posix_shell_syntax() -> None:
    completed = subprocess.run(
        ["sh", "-n", str(ENTRYPOINT)],
        check=False,
        capture_output=True,
        text=True,
    )

    assert completed.returncode == 0, completed.stderr
