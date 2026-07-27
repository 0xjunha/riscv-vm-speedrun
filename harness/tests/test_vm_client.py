from __future__ import annotations

import os
import threading
import time
from pathlib import Path

import pytest

from rv32im_harness.vm_client import (
    VmError,
    VmProcessError,
    VmServer,
    VmTimeout,
    _run_command,
    run_once,
)
from rv32im_harness.vm_interface import (
    ProtocolError,
    ProtocolResponseError,
    RunOutcome,
    Status,
)


@pytest.fixture
def stub_vm(monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.delenv("STUB_VM_MODE", raising=False)
    monkeypatch.delenv("STUB_VM_LOG", raising=False)
    monkeypatch.delenv("STUB_VM_PID_FILE", raising=False)
    return Path(__file__).with_name("stub_vm.py")


def assert_process_gone(pid: int) -> None:
    deadline = time.monotonic() + 2
    while True:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return
        if time.monotonic() >= deadline:
            pytest.fail(f"process {pid} is still running")
        time.sleep(0.01)


def assert_capture_threads_stopped() -> None:
    assert not any(thread.name == "rv32im-capture" for thread in threading.enumerate())


def assert_server_stopped(server: VmServer) -> None:
    with pytest.raises(VmError, match="closed"):
        server.reset()
    assert_process_gone(server._transport.process.pid)
    assert_capture_threads_stopped()


def test_run_once_returns_output_and_state(stub_vm: Path) -> None:
    outcome = run_once(
        stub_vm,
        b"ELF",
        b"input",
        instruction_limit=12,
        output_limit=32,
        capture_state=True,
        inspections=((0x10000, 4), (0x0400_0000, 0)),
    )

    assert isinstance(outcome, RunOutcome)
    assert outcome.output == b"input"
    assert outcome.result.exit_code == 0
    assert outcome.result.retired_instructions == 3
    assert outcome.state is not None
    assert outcome.state.pc == 0x10000
    assert [(item.address, item.data) for item in outcome.state.memory] == [
        (0x10000, b"\x00\x01\x02\x03"),
        (0x0400_0000, b""),
    ]


def test_run_once_reports_host_failure(
    stub_vm: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("STUB_VM_MODE", "host-error")

    with pytest.raises(VmProcessError, match="status 7.*deliberate host failure"):
        run_once(stub_vm, b"ELF")


def test_command_capture_is_bounded_while_draining(
    stub_vm: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setenv("STUB_VM_MODE", "flood-host-error")
    completed = _run_command(
        stub_vm,
        [
            "run",
            "--elf",
            "elf",
            "--input",
            "input",
            "--output",
            "output",
            "--result",
            "result",
            "--instruction-limit",
            "3",
            "--output-limit",
            "0",
        ],
        cwd=tmp_path,
        timeout=2,
    )

    assert completed.returncode == 7
    assert len(completed.stdout) == 64 * 1024
    assert len(completed.stderr) == 64 * 1024
    assert not any(thread.name == "rv32im-capture" for thread in threading.enumerate())


@pytest.mark.parametrize("timeout", [float("nan"), float("inf"), float("-inf")])
def test_public_apis_reject_nonfinite_timeout(stub_vm: Path, timeout: float) -> None:
    with pytest.raises(ValueError, match="positive finite"):
        run_once(stub_vm, b"ELF", timeout=timeout)
    with pytest.raises(ValueError, match="positive finite"):
        VmServer(stub_vm, timeout=timeout)


@pytest.mark.parametrize(
    ("mode", "error", "message"),
    [
        ("missing-result", VmError, "result JSON was not produced"),
        ("malformed-result", ProtocolError, "normative keys"),
        ("oversized-output", VmError, "guest output exceeds"),
        ("nonregular-output", VmError, "guest output is not a regular file"),
    ],
)
def test_run_once_rejects_bad_artifacts(
    stub_vm: Path,
    monkeypatch: pytest.MonkeyPatch,
    mode: str,
    error: type[Exception],
    message: str,
) -> None:
    monkeypatch.setenv("STUB_VM_MODE", mode)

    with pytest.raises(error, match=message):
        run_once(stub_vm, b"ELF", output_limit=4)


@pytest.mark.parametrize(
    ("mode", "error", "message"),
    [
        ("missing-state", VmError, "state JSON was not produced"),
        ("nonregular-state", VmError, "state JSON is not a regular file"),
        ("oversized-state", VmError, "state JSON exceeds"),
        ("state-output-mismatch", ProtocolError, "state output_length"),
        ("state-retirement-mismatch", ProtocolError, "state retirement count"),
        ("state-ranges-mismatch", ProtocolError, "state memory ranges"),
    ],
)
def test_run_once_rejects_bad_state_artifacts(
    stub_vm: Path,
    monkeypatch: pytest.MonkeyPatch,
    mode: str,
    error: type[Exception],
    message: str,
) -> None:
    monkeypatch.setenv("STUB_VM_MODE", mode)

    with pytest.raises(error, match=message):
        run_once(
            stub_vm,
            b"ELF",
            capture_state=True,
            inspections=((0x10000, 4),),
        )


def test_run_once_timeout_reaps_process_group(
    stub_vm: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    pid_file = tmp_path / "pids"
    monkeypatch.setenv("STUB_VM_MODE", "hang-with-child")
    monkeypatch.setenv("STUB_VM_PID_FILE", os.fspath(pid_file))

    with pytest.raises(VmTimeout):
        run_once(stub_vm, b"ELF", timeout=2)

    pids = [int(line) for line in pid_file.read_text().splitlines()]
    assert len(pids) == 2
    for pid in pids:
        assert_process_gone(pid)
    assert_capture_threads_stopped()


def test_run_once_rejects_retirement_above_limit(
    stub_vm: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("STUB_VM_MODE", "too-many-retired")
    with pytest.raises(ProtocolError, match="instruction limit"):
        run_once(stub_vm, b"ELF", instruction_limit=2)


def test_server_lifecycle(
    stub_vm: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    log = tmp_path / "requests"
    monkeypatch.setenv("STUB_VM_LOG", os.fspath(log))

    with VmServer(stub_vm) as server:
        with pytest.raises(ProtocolResponseError) as caught:
            server.run()
        assert caught.value.status is Status.INVALID_STATE
        server.load(b"ELF")
        outcome = server.run(b"abc", instruction_limit=20, output_limit=10)
        assert isinstance(outcome, RunOutcome)
        assert outcome.output == b"abc"
        assert outcome.state is None
        server.reset()
        server.unload()

    assert log.read_text().splitlines() == [
        "load",
        "run",
        "reset",
        "unload",
        "shutdown",
    ]


def test_server_drains_bounded_stderr(
    stub_vm: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("STUB_VM_MODE", "server-stderr-flood")
    with VmServer(stub_vm, timeout=2) as server:
        server.load(b"ELF")
        server.unload()

    assert len(server._transport.stderr()) == 64 * 1024
    assert not any(thread.name == "rv32im-capture" for thread in threading.enumerate())


@pytest.mark.parametrize(
    ("input_data", "instruction_limit", "output_limit", "message"),
    [
        (b"", 2, 10, "instruction limit"),
        (b"abc", 10, 2, "output limit"),
    ],
)
def test_server_rejects_response_above_requested_limits(
    stub_vm: Path,
    input_data: bytes,
    instruction_limit: int,
    output_limit: int,
    message: str,
) -> None:
    server = VmServer(stub_vm)
    server.load(b"ELF")

    with pytest.raises(ProtocolError, match=message):
        server.run(
            input_data,
            instruction_limit=instruction_limit,
            output_limit=output_limit,
        )

    assert_server_stopped(server)


def test_server_closes_after_malformed_run_response(
    stub_vm: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("STUB_VM_MODE", "run-malformed-success")
    server = VmServer(stub_vm)
    server.load(b"ELF")

    with pytest.raises(ProtocolError, match="RUN response is shorter"):
        server.run()

    assert_server_stopped(server)


@pytest.mark.parametrize(
    ("mode", "operation"),
    [
        ("load-success-payload", "load"),
        ("reset-success-payload", "reset"),
        ("unload-success-payload", "unload"),
        ("shutdown-success-payload", "shutdown"),
    ],
)
def test_server_closes_after_nonempty_success_response(
    stub_vm: Path,
    monkeypatch: pytest.MonkeyPatch,
    mode: str,
    operation: str,
) -> None:
    monkeypatch.setenv("STUB_VM_MODE", mode)
    server = VmServer(stub_vm)
    if operation in {"reset", "unload"}:
        server.load(b"ELF")

    with pytest.raises(ProtocolError, match=f"{operation.upper()} success response"):
        if operation == "load":
            server.load(b"ELF")
        else:
            getattr(server, operation)()

    assert_server_stopped(server)


@pytest.mark.parametrize(
    ("status", "expected"),
    [
        (Status.MALFORMED_FRAME, "MalformedFrame"),
        (Status.FRAME_TOO_LARGE, "FrameTooLarge"),
        (Status.INTERNAL_ERROR, "InternalError"),
    ],
)
def test_server_closes_after_terminal_status(
    stub_vm: Path,
    monkeypatch: pytest.MonkeyPatch,
    status: Status,
    expected: str,
) -> None:
    monkeypatch.setenv("STUB_VM_MODE", f"terminal-{int(status)}")
    server = VmServer(stub_vm)

    with pytest.raises(ProtocolResponseError, match=expected):
        server.load(b"ELF")
    with pytest.raises(VmError, match="closed"):
        server.reset()
    server.close()


def test_bad_ready_cleans_up_server_process(
    stub_vm: Path,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    pid_file = tmp_path / "pid"
    monkeypatch.setenv("STUB_VM_MODE", "bad-ready")
    monkeypatch.setenv("STUB_VM_PID_FILE", os.fspath(pid_file))

    with pytest.raises(ProtocolError, match="READY"):
        VmServer(stub_vm)

    assert_process_gone(int(pid_file.read_text()))
    assert_capture_threads_stopped()


def test_bad_response_cleans_up_server_process(
    stub_vm: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("STUB_VM_MODE", "bad-correlation")
    server = VmServer(stub_vm)
    pid = server._transport.process.pid

    with pytest.raises(ProtocolError, match="request_id"):
        server.load(b"ELF")

    with pytest.raises(VmError, match="closed"):
        server.reset()
    assert_process_gone(pid)
    assert_capture_threads_stopped()


def test_server_request_timeout_cleans_up_process(
    stub_vm: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("STUB_VM_MODE", "server-hang")
    server = VmServer(stub_vm, timeout=0.5)
    pid = server._transport.process.pid

    with pytest.raises(VmTimeout):
        server.load(b"ELF")

    with pytest.raises(VmError, match="closed"):
        server.reset()
    assert_process_gone(pid)
    assert_capture_threads_stopped()


def test_server_write_timeout_cleans_up_process(
    stub_vm: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("STUB_VM_MODE", "server-no-read")
    server = VmServer(stub_vm, timeout=0.5)
    pid = server._transport.process.pid

    with pytest.raises(VmTimeout, match="server write"):
        server.load(bytes(8 * 1024 * 1024))

    with pytest.raises(VmError, match="closed"):
        server.reset()
    assert_process_gone(pid)
    assert_capture_threads_stopped()


@pytest.mark.parametrize(
    ("mode", "error", "message"),
    [
        ("shutdown-hang", VmTimeout, "server exit"),
        ("shutdown-nonzero", VmProcessError, "status 7"),
        ("shutdown-trailing-output", ProtocolError, "trailing bytes"),
    ],
)
def test_server_rejects_invalid_shutdown(
    stub_vm: Path,
    monkeypatch: pytest.MonkeyPatch,
    mode: str,
    error: type[Exception],
    message: str,
) -> None:
    monkeypatch.setenv("STUB_VM_MODE", mode)
    server = VmServer(stub_vm, timeout=0.5)
    pid = server._transport.process.pid

    with pytest.raises(error, match=message):
        server.close()

    with pytest.raises(VmError, match="closed"):
        server.reset()
    assert_process_gone(pid)
    assert_capture_threads_stopped()
