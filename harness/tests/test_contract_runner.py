from __future__ import annotations

import hashlib
import json
import os
import time
from pathlib import Path

import pytest

from rv32im_harness import contract_runner
from rv32im_harness.contract_runner import (
    ContractFailure,
    _Case,
    _load_cases,
    _require_result,
    _require_state,
    run_contracts,
)
from rv32im_harness.interface_contracts import (
    InterfaceFailure,
    _CliResult,
    _RawServer,
    _regular_bytes,
    _require_cli_exit,
    _run_source_alias_contracts,
)
from rv32im_harness.vm_client import VmError
from rv32im_harness.vm_interface import (
    MemoryRange,
    RunOutcome,
    RunResult,
    Trap,
    VMState,
)

STUB_VM = Path(__file__).with_name("stub_vm.py")


def _manifest(root: Path) -> Path:
    elf = b"ELF"
    elf_path = root / "contracts/artifacts/elf/case.elf"
    elf_path.parent.mkdir(parents=True)
    elf_path.write_bytes(elf)
    manifest = root / "contracts/artifacts/manifest.json"
    manifest.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "cases": [
                    {
                        "id": "case",
                        "kind": "execute",
                        "symbols": {"fault": 0x10004},
                        "elf": "contracts/artifacts/elf/case.elf",
                        "elf_sha256": hashlib.sha256(elf).hexdigest(),
                    }
                ],
            }
        )
    )
    return manifest


def test_load_cases_verifies_artifact_hash(tmp_path: Path) -> None:
    manifest = _manifest(tmp_path)

    assert _load_cases(manifest)[0].elf == b"ELF"
    (tmp_path / "contracts/artifacts/elf/case.elf").write_bytes(b"changed")
    with pytest.raises(ContractFailure, match="hash"):
        _load_cases(manifest)


def test_load_cases_rejects_non_object_manifest(tmp_path: Path) -> None:
    manifest = tmp_path / "contracts/artifacts/manifest.json"
    manifest.parent.mkdir(parents=True)
    manifest.write_text("[]")

    with pytest.raises(ContractFailure, match="schema"):
        _load_cases(manifest)


def test_expected_trap_and_state_can_use_symbols() -> None:
    case = _Case(
        "case",
        "execute",
        b"ELF",
        {"symbols": {"fault": 0x10004}},
    )
    run = {
        "output_hex": "61",
        "result": {
            "status": "trap",
            "trap": {
                "cause": "IllegalInstruction",
                "pc": "fault",
                "value": 1,
            },
            "retired_instructions": 2,
        },
        "state": {
            "pc": "fault",
            "registers": {"5": 7},
            "memory": [{"address": 9, "data_hex": "0102"}],
        },
    }
    registers = [0] * 32
    registers[5] = 7
    outcome = RunOutcome(
        RunResult(
            1,
            "trap",
            None,
            Trap("IllegalInstruction", 0x10004, 1),
            None,
            2,
            1,
        ),
        b"a",
        VMState(
            1,
            0x10004,
            tuple(registers),
            (MemoryRange(9, b"\x01\x02"),),
            2,
            1,
        ),
    )

    _require_result(case, run, outcome)
    _require_state(case, run, outcome)


def test_run_contracts_combines_generated_and_procedural_checks(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    cases = [
        _Case("image-a", "execute", b"A", {"runs": [], "symbols": {}}),
        _Case("image-b", "execute", b"B", {"runs": [], "symbols": {}}),
        _Case("bad", "reject", b"bad", {"symbols": {}}),
    ]
    monkeypatch.setattr(contract_runner, "_load_cases", lambda _path: cases)
    monkeypatch.setattr(
        contract_runner,
        "_run_executable_cases",
        lambda _executable, selected: len(selected),
    )
    monkeypatch.setattr(
        contract_runner,
        "_run_rejected_cases",
        lambda _executable, selected, _valid: len(selected),
    )
    monkeypatch.setattr(
        contract_runner,
        "_run_image_lifecycle",
        lambda _executable, _by_id: 3,
    )
    monkeypatch.setattr(
        contract_runner,
        "run_interface_contracts",
        lambda _executable, _assets: 5,
    )

    assert run_contracts("unused") == 11


def test_raw_server_cleans_up_after_invalid_ready(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    pid_file = tmp_path / "pid"
    monkeypatch.setenv("STUB_VM_MODE", "bad-ready")
    monkeypatch.setenv("STUB_VM_PID_FILE", str(pid_file))

    with pytest.raises(ValueError, match="READY"):
        _RawServer(STUB_VM)

    pid = int(pid_file.read_text())
    for _ in range(100):
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            break
        time.sleep(0.01)
    else:
        raise AssertionError("server process leaked after invalid READY")


def test_source_alias_check_detects_early_destination_truncation(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("STUB_VM_MODE", "contract-alias")
    _run_source_alias_contracts(STUB_VM, b"ELF")

    monkeypatch.setenv("STUB_VM_MODE", "contract-alias-bug")
    with pytest.raises(InterfaceFailure, match="input source"):
        _run_source_alias_contracts(STUB_VM, b"ELF")

    monkeypatch.setenv("STUB_VM_MODE", "contract-result-alias-bug")
    with pytest.raises(InterfaceFailure, match="ELF source"):
        _run_source_alias_contracts(STUB_VM, b"ELF")


def test_cli_outcome_check_rejects_wrong_boundary_result() -> None:
    result = _CliResult(
        0,
        b"",
        b"",
        (
            b'{"schema_version":1,"status":"exit","exit_code":0,"trap":null,'
            b'"resource_failure":null,"retired_instructions":1,"output_length":0}'
        ),
        None,
    )

    with pytest.raises(InterfaceFailure, match="differs"):
        _require_cli_exit(result, "boundary", exit_code=7, retired=3)


def test_procedural_file_read_rejects_symlink(
    tmp_path: Path,
) -> None:
    target = tmp_path / "target"
    target.write_bytes(b"x")
    link = tmp_path / "link"
    link.symlink_to(target)

    with pytest.raises(VmError, match="regular file"):
        _regular_bytes(link, 10, "test file")
