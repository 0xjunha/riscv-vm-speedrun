from __future__ import annotations

import hashlib
import json
from pathlib import Path

import pytest

from rv32im_harness.conformance import (
    ConformanceFailure,
    _require_success,
    run_conformance,
)
from rv32im_harness.vm_interface import RunOutcome, RunResult, Trap

STUB_VM = Path(__file__).with_name("stub_vm.py")


def _outcome(
    *, status: str = "exit", exit_code: int | None = 0, output: bytes = b""
) -> RunOutcome:
    trap = Trap("IllegalInstruction", 0, 0) if status == "trap" else None
    result = RunResult(1, status, exit_code, trap, None, 1, len(output))
    return RunOutcome(result, output, None)


def _manifest(root: Path) -> Path:
    records = []
    for suite, identifier in (("act4", "add"), ("riscv-tests", "sub")):
        elf = f"conformance/artifacts/elf/{suite}/{identifier}.elf"
        payload = f"{suite}/{identifier}".encode()
        path = root / elf
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(payload)
        records.append(
            {
                "suite": suite,
                "id": identifier,
                "elf": elf,
                "elf_sha256": hashlib.sha256(payload).hexdigest(),
            }
        )

    manifest = root / "conformance/artifacts/manifest.json"
    manifest.write_text(json.dumps({"schema_version": 1, "cases": records}))
    return manifest


def test_run_conformance_uses_both_interfaces(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    log = tmp_path / "requests"
    monkeypatch.setenv("STUB_VM_LOG", str(log))

    assert run_conformance(STUB_VM, _manifest(tmp_path)) == 2
    assert log.read_text().splitlines() == [
        "run-once",
        "run-once",
        "load",
        "run",
        "unload",
        "load",
        "run",
        "unload",
        "shutdown",
    ]


def test_run_conformance_rejects_changed_elf(tmp_path: Path) -> None:
    manifest = _manifest(tmp_path)
    elf = tmp_path / "conformance/artifacts/elf/act4/add.elf"
    elf.write_bytes(b"changed")

    with pytest.raises(ConformanceFailure, match="hash"):
        run_conformance(STUB_VM, manifest)


@pytest.mark.parametrize(
    ("outcome", "message"),
    [
        (_outcome(status="trap", exit_code=None), "expected exit"),
        (_outcome(exit_code=1), "exit code 0"),
        (_outcome(output=b"x"), "empty output"),
    ],
)
def test_require_success_checks_self_checking_result(
    outcome: RunOutcome, message: str
) -> None:
    with pytest.raises(ConformanceFailure, match=message):
        _require_success("one-shot", "suite/case", outcome)
