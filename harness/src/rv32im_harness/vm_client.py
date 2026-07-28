"""Launch and drive an rv32vm executable.

Supports the one-shot ``rv32vm run`` and persistent ``rv32vm serve`` interfaces,
including timeouts, bounded diagnostics, and process cleanup.
"""

from __future__ import annotations

import math
import os
import select
import signal
import stat
import subprocess
import tempfile
import threading
import time
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from types import TracebackType
from typing import BinaryIO, Self

from .vm_interface import (
    ADDRESS_SPACE_SIZE,
    DEFAULT_INSTRUCTION_LIMIT,
    DEFAULT_OUTPUT_LIMIT,
    MAX_ELF_SIZE,
    MAX_INSPECTION_BYTES,
    MAX_INSPECTION_COUNT,
    MAX_OUTPUT_LIMIT,
    MAX_PAYLOAD_SIZE,
    MESSAGE_HEADER_LAYOUT,
    MessageHeader,
    Opcode,
    ProtocolError,
    ProtocolResponseError,
    RunOutcome,
    RunResult,
    Status,
    VMState,
    decode_ready,
    decode_response,
    decode_run_response,
    encode_request,
    encode_run_request,
)

_MAX_DIAGNOSTIC_SIZE = 64 * 1024  # 64 KiB retained from stdout or stderr.
_MAX_RESULT_SIZE = 64 * 1024  # 64 KiB result JSON harness limit.
_MAX_STATE_SIZE = 12 * 1024 * 1024  # 12 MiB state JSON harness limit.
_DEFAULT_TIMEOUT = 10.0  # 10 seconds per VM operation.


class VmError(RuntimeError):
    """The host process failed to provide a completed, valid VM operation."""


class VmTimeout(VmError):
    """The VM did not complete an operation before its deadline."""


class VmProcessError(VmError):
    """The VM process could not start or exited unexpectedly."""


@dataclass(frozen=True)
class _CommandResult:
    """Bounded result of one package-internal VM command invocation."""

    returncode: int
    stdout: bytes
    stderr: bytes


def _validate_run_limits(
    outcome: RunOutcome,
    instruction_limit: int,
    output_limit: int,
) -> RunOutcome:
    if outcome.result.retired_instructions > instruction_limit:
        raise ProtocolError("result exceeds the requested instruction limit")
    if len(outcome.output) > output_limit or len(outcome.output) > MAX_OUTPUT_LIMIT:
        raise ProtocolError("response exceeds the requested output limit")
    return outcome


def _positive_timeout(value: float) -> float:
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not math.isfinite(value)
        or value <= 0
    ):
        raise ValueError("timeout must be a positive finite number")
    return float(value)


def _vm_path(executable: str | os.PathLike[str]) -> Path:
    path = Path(executable).expanduser().resolve()
    if not path.is_file():
        raise ValueError(f"VM executable is not a regular file: {path}")
    if not os.access(path, os.X_OK):
        raise ValueError(f"VM executable is not executable: {path}")
    return path


def _environment(work_directory: Path) -> dict[str, str]:
    environment = os.environ.copy()
    environment["HOME"] = os.fspath(work_directory)
    environment["TMPDIR"] = os.fspath(work_directory)
    return environment


def _kill_process_group(process: subprocess.Popen[bytes]) -> None:
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


class _BoundedCapture:
    """Continuously drain a pipe while retaining at most 64 KiB."""

    def __init__(self, stream: BinaryIO) -> None:
        self._stream = stream
        self._data = bytearray()
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._finished = False
        os.set_blocking(stream.fileno(), False)
        self._thread = threading.Thread(
            target=self._drain,
            name="rv32im-capture",
            daemon=True,
        )
        self._thread.start()

    def _drain(self) -> None:
        try:
            descriptor = self._stream.fileno()
            while not self._stop.is_set():
                readable, _, _ = select.select([descriptor], [], [], 0.05)
                if not readable:
                    continue
                try:
                    chunk = os.read(descriptor, 64 * 1024)
                except BlockingIOError:
                    continue
                if not chunk:
                    return
                with self._lock:
                    remaining = _MAX_DIAGNOSTIC_SIZE - len(self._data)
                    self._data.extend(chunk[:remaining])
        except OSError:
            pass

    def snapshot(self) -> bytes:
        with self._lock:
            return bytes(self._data)

    def finish(self) -> bytes:
        if not self._finished:
            self._thread.join(timeout=0.25)
            if self._thread.is_alive():
                self._stop.set()
                self._thread.join()
            self._stream.close()
            self._finished = True
        return self.snapshot()


def _diagnostic(data: bytes) -> str:
    text = data.decode("utf-8", errors="replace")
    if len(data) == _MAX_DIAGNOSTIC_SIZE:
        text += "\n... diagnostic output may be truncated"
    return text


def _run_command(
    executable: str | os.PathLike[str],
    arguments: Sequence[str],
    *,
    cwd: str | os.PathLike[str],
    timeout: float = _DEFAULT_TIMEOUT,
) -> _CommandResult:
    """Invoke a VM command without retaining unbounded output in memory."""

    vm = _vm_path(executable)
    deadline_seconds = _positive_timeout(timeout)
    work_directory = Path(cwd)
    try:
        process = subprocess.Popen(
            [os.fspath(vm), *arguments],
            cwd=work_directory,
            env=_environment(work_directory),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
    except OSError as error:
        raise VmProcessError(f"failed to start {vm}: {error}") from error
    assert process.stdout is not None and process.stderr is not None
    stdout_capture = _BoundedCapture(process.stdout)
    stderr_capture = _BoundedCapture(process.stderr)
    timed_out = False
    try:
        try:
            returncode = process.wait(timeout=deadline_seconds)
        except subprocess.TimeoutExpired:
            timed_out = True
            _kill_process_group(process)
            returncode = process.wait()
        finally:
            # Descendants belong to this invocation and must not leak into the
            # next run or keep a capture pipe open.
            _kill_process_group(process)
    finally:
        if process.poll() is None:
            _kill_process_group(process)
            process.wait()
        stdout = stdout_capture.finish()
        stderr = stderr_capture.finish()
    if timed_out:
        detail = _diagnostic(stderr)
        suffix = f": {detail}" if detail else ""
        raise VmTimeout(f"VM command exceeded {deadline_seconds:g} seconds{suffix}")
    return _CommandResult(returncode, stdout, stderr)


def _read_regular_file(path: Path, maximum_size: int, description: str) -> bytes:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise VmError(f"{description} was not produced: {error}") from error
    if not stat.S_ISREG(metadata.st_mode):
        raise VmError(f"{description} is not a regular file")
    if metadata.st_size > maximum_size:
        raise VmError(f"{description} exceeds its {maximum_size}-byte harness bound")
    try:
        with path.open("rb") as stream:
            data = stream.read(maximum_size + 1)
    except OSError as error:
        raise VmError(f"cannot read {description}: {error}") from error
    if len(data) > maximum_size:
        raise VmError(f"{description} exceeds its {maximum_size}-byte harness bound")
    return data


def _inspection_range(value: tuple[int, int]) -> tuple[int, int]:
    try:
        address, length = value
    except (TypeError, ValueError) as error:
        raise ValueError("each inspection must be an (address, length) pair") from error
    if (
        isinstance(address, bool)
        or not isinstance(address, int)
        or isinstance(length, bool)
        or not isinstance(length, int)
        or address < 0
        or length < 0
        or address > ADDRESS_SPACE_SIZE
        or address + length > ADDRESS_SPACE_SIZE
    ):
        raise ValueError("inspection range is outside the guest address space")
    return address, length


def run_once(
    executable: str | os.PathLike[str],
    elf: bytes,
    input_data: bytes = b"",
    *,
    instruction_limit: int = DEFAULT_INSTRUCTION_LIMIT,
    output_limit: int = DEFAULT_OUTPUT_LIMIT,
    capture_state: bool = False,
    inspections: Sequence[tuple[int, int]] = (),
    timeout: float = _DEFAULT_TIMEOUT,
) -> RunOutcome:
    """Execute one completed run through the file-oriented VM interface."""

    if not isinstance(elf, bytes):
        raise TypeError("elf must be bytes")
    if len(elf) > MAX_ELF_SIZE:
        raise ValueError("ELF exceeds the 8 MiB maximum")
    # Reuse the protocol's normative validation for limits and input size.
    encode_run_request(instruction_limit, output_limit, input_data)
    ranges = tuple(_inspection_range(value) for value in inspections)
    if ranges and not capture_state:
        raise ValueError("inspections require capture_state=True")
    if len(ranges) > MAX_INSPECTION_COUNT:
        raise ValueError("inspection count exceeds 1024 ranges")
    if sum(length for _, length in ranges) > MAX_INSPECTION_BYTES:
        raise ValueError("aggregate inspection length exceeds 8 MiB")

    with tempfile.TemporaryDirectory(prefix="rv32im-run-") as temporary:
        work_directory = Path(temporary)
        elf_path = work_directory / "program.elf"
        input_path = work_directory / "input.bin"
        output_path = work_directory / "output.bin"
        result_path = work_directory / "result.json"
        state_path = work_directory / "state.json"
        elf_path.write_bytes(elf)
        input_path.write_bytes(input_data)
        output_path.write_bytes(b"stale output")
        result_path.write_bytes(b"stale result")

        arguments = [
            "run",
            "--elf",
            elf_path.name,
            "--input",
            input_path.name,
            "--output",
            output_path.name,
            "--result",
            result_path.name,
            "--instruction-limit",
            str(instruction_limit),
            "--output-limit",
            str(output_limit),
        ]
        if capture_state:
            state_path.write_bytes(b"stale state")
            arguments.extend(("--state", state_path.name))
            for address, length in ranges:
                arguments.extend(("--inspect", f"0x{address:x}:0x{length:x}"))

        completed = _run_command(
            executable,
            arguments,
            cwd=work_directory,
            timeout=timeout,
        )
        if completed.returncode != 0:
            detail = _diagnostic(completed.stderr)
            suffix = f": {detail}" if detail else ""
            raise VmProcessError(
                f"rv32vm run exited with status {completed.returncode}{suffix}"
            )
        if completed.stdout:
            raise VmError("rv32vm run wrote bytes to standard output")

        output = _read_regular_file(output_path, output_limit, "guest output")
        result = RunResult.from_json_bytes(
            _read_regular_file(result_path, _MAX_RESULT_SIZE, "result JSON")
        )
        if result.output_length != len(output):
            raise ProtocolError("result output_length does not match the output file")

        state_value = None
        if capture_state:
            state_value = VMState.from_json_bytes(
                _read_regular_file(state_path, _MAX_STATE_SIZE, "state JSON")
            )
            if state_value.output_length != len(output):
                raise ProtocolError(
                    "state output_length does not match the output file"
                )
            if state_value.retired_instructions != result.retired_instructions:
                raise ProtocolError("state retirement count does not match the result")
            actual_ranges = tuple(
                (item.address, len(item.data)) for item in state_value.memory
            )
            if actual_ranges != ranges:
                raise ProtocolError(
                    "state memory ranges do not match the requested inspections"
                )
        return _validate_run_limits(
            RunOutcome(result, output, state_value),
            instruction_limit,
            output_limit,
        )


class _ServerTransport:
    """Deadline-aware byte transport for ``VmServer``."""

    def __init__(
        self,
        executable: str | os.PathLike[str],
        *,
        timeout: float = _DEFAULT_TIMEOUT,
    ) -> None:
        self.timeout = _positive_timeout(timeout)
        self._temporary = tempfile.TemporaryDirectory(prefix="rv32im-serve-")
        self.work_directory = Path(self._temporary.name)
        self._closed = False
        vm = _vm_path(executable)
        try:
            self.process = subprocess.Popen(
                [os.fspath(vm), "serve"],
                cwd=self.work_directory,
                env=_environment(self.work_directory),
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                start_new_session=True,
            )
        except OSError as error:
            self._temporary.cleanup()
            raise VmProcessError(f"failed to start {vm}: {error}") from error
        assert self.process.stdin is not None
        assert self.process.stdout is not None
        assert self.process.stderr is not None
        self._stdin = self.process.stdin
        self._stdout = self.process.stdout
        self._stderr_capture = _BoundedCapture(self.process.stderr)
        os.set_blocking(self._stdin.fileno(), False)
        os.set_blocking(self._stdout.fileno(), False)

    def deadline(self, timeout: float | None = None) -> float:
        return time.monotonic() + (
            self.timeout if timeout is None else _positive_timeout(timeout)
        )

    def _remaining(self, deadline: float, operation: str) -> float:
        remaining = deadline - time.monotonic()
        if remaining > 0:
            return remaining
        self.abort()
        raise VmTimeout(f"{operation} exceeded its deadline")

    def _process_error(self, operation: str) -> VmProcessError:
        returncode = self.process.poll()
        detail = _diagnostic(self._stderr_capture.snapshot())
        status = "before exiting" if returncode is None else f"with status {returncode}"
        suffix = f": {detail}" if detail else ""
        return VmProcessError(f"VM server ended {status} during {operation}{suffix}")

    def write(
        self,
        data: bytes,
        *,
        deadline: float | None = None,
    ) -> None:
        if self._closed:
            raise VmError("VM server transport is closed")
        limit = self.deadline() if deadline is None else deadline
        view = memoryview(data)
        while view:
            try:
                _, writable, _ = select.select(
                    [], [self._stdin], [], self._remaining(limit, "server write")
                )
            except (OSError, ValueError) as error:
                raise self._process_error("server write") from error
            if not writable:
                continue
            try:
                written = os.write(self._stdin.fileno(), view)
            except (BrokenPipeError, OSError) as error:
                raise self._process_error("server write") from error
            view = view[written:]

    def read_exact(
        self,
        length: int,
        *,
        deadline: float | None = None,
    ) -> bytes:
        if isinstance(length, bool) or not isinstance(length, int) or length < 0:
            raise ValueError("read length must be a nonnegative integer")
        if self._closed:
            raise VmError("VM server transport is closed")
        limit = self.deadline() if deadline is None else deadline
        chunks = bytearray()
        while len(chunks) < length:
            try:
                readable, _, _ = select.select(
                    [self._stdout], [], [], self._remaining(limit, "server read")
                )
            except (OSError, ValueError) as error:
                raise self._process_error("server read") from error
            if not readable:
                continue
            try:
                chunk = os.read(self._stdout.fileno(), length - len(chunks))
            except OSError as error:
                raise self._process_error("server read") from error
            if not chunk:
                raise self._process_error("server read")
            chunks.extend(chunk)
        return bytes(chunks)

    def read_frame(
        self,
        *,
        deadline: float | None = None,
    ) -> tuple[MessageHeader, bytes]:
        limit = self.deadline() if deadline is None else deadline
        header = MessageHeader.decode(
            self.read_exact(MESSAGE_HEADER_LAYOUT.size, deadline=limit)
        )
        if header.payload_length > MAX_PAYLOAD_SIZE:
            raise ProtocolError("response exceeds the 8 MiB protocol maximum")
        payload = self.read_exact(header.payload_length, deadline=limit)
        return header, payload

    def wait(self, *, deadline: float | None = None) -> int:
        limit = self.deadline() if deadline is None else deadline
        try:
            returncode = self.process.wait(
                timeout=self._remaining(limit, "server exit")
            )
        except subprocess.TimeoutExpired:
            self.abort()
            raise VmTimeout("server exit exceeded its deadline") from None
        finally:
            if self.process.poll() is not None:
                _kill_process_group(self.process)
                self._stderr_capture.finish()
        return returncode

    def require_eof(self, *, deadline: float | None = None) -> None:
        limit = self.deadline() if deadline is None else deadline
        while True:
            try:
                readable, _, _ = select.select(
                    [self._stdout], [], [], self._remaining(limit, "stdout drain")
                )
            except (OSError, ValueError) as error:
                raise self._process_error("stdout drain") from error
            if not readable:
                continue
            if os.read(self._stdout.fileno(), 1):
                raise ProtocolError("server wrote trailing bytes after SHUTDOWN")
            return

    def stderr(self) -> bytes:
        return self._stderr_capture.snapshot()

    def close_input(self) -> None:
        """Send EOF to a server without closing its output stream."""

        if not self._stdin.closed:
            self._stdin.close()

    def abort(self) -> None:
        if self._closed:
            return
        _kill_process_group(self.process)
        try:
            self.process.wait(timeout=1)
        except subprocess.TimeoutExpired:
            _kill_process_group(self.process)
            self.process.wait()

    def close(self) -> None:
        if self._closed:
            return
        try:
            self.abort()
        finally:
            self._stdin.close()
            self._stdout.close()
            self._stderr_capture.finish()
            self._temporary.cleanup()
            self._closed = True


class VmServer:
    """Start and control one VM through its persistent interface.

    Construction runs ``executable serve`` and waits until the VM reports
    that it is ready. Use ``load()``, ``run()``, ``reset()``, and ``unload()``
    to execute multiple runs in that same process. Closing the client requests
    a clean shutdown; a timeout or invalid response terminates the process.
    """

    def __init__(
        self,
        executable: str | os.PathLike[str],
        *,
        timeout: float = _DEFAULT_TIMEOUT,
    ) -> None:
        self._transport = _ServerTransport(executable, timeout=timeout)
        self._next_request_id = 1
        self._shutdown = False
        self._closed = False
        try:
            deadline = self._transport.deadline()
            header, payload = self._transport.read_frame(deadline=deadline)
            decode_ready(header, payload)
        except BaseException:
            self._transport.close()
            raise

    def _request(
        self,
        opcode: Opcode,
        payload: bytes = b"",
        *,
        deadline: float | None = None,
    ) -> bytes:
        if self._closed or self._shutdown:
            raise VmError("VM server is closed")
        limit = self._transport.deadline() if deadline is None else deadline
        request_id = self._next_request_id
        self._next_request_id = (request_id + 1) & 0xFFFF_FFFF
        try:
            self._transport.write(
                encode_request(opcode, request_id, payload), deadline=limit
            )
            header, response = self._transport.read_frame(deadline=limit)
            return decode_response(
                header, response, opcode=opcode, request_id=request_id
            )
        except ProtocolResponseError as error:
            if error.status in {
                Status.MALFORMED_FRAME,
                Status.FRAME_TOO_LARGE,
                Status.INTERNAL_ERROR,
            }:
                self.abort()
            raise
        except BaseException:
            self.abort()
            raise

    def _request_empty(
        self,
        opcode: Opcode,
        payload: bytes = b"",
        *,
        deadline: float | None = None,
    ) -> None:
        if self._request(opcode, payload, deadline=deadline):
            self.abort()
            raise ProtocolError(f"{opcode.name} success response must be empty")

    def load(self, elf: bytes) -> None:
        if not isinstance(elf, bytes):
            raise TypeError("elf must be bytes")
        if not elf:
            raise ValueError("ELF payload must not be empty")
        if len(elf) > MAX_ELF_SIZE:
            raise ValueError("ELF exceeds the 8 MiB maximum")
        self._request_empty(Opcode.LOAD, elf)

    def run(
        self,
        input_data: bytes = b"",
        *,
        instruction_limit: int = DEFAULT_INSTRUCTION_LIMIT,
        output_limit: int = DEFAULT_OUTPUT_LIMIT,
    ) -> RunOutcome:
        payload = encode_run_request(instruction_limit, output_limit, input_data)
        response = self._request(Opcode.RUN, payload)
        try:
            return _validate_run_limits(
                decode_run_response(response),
                instruction_limit,
                output_limit,
            )
        except BaseException:
            self.abort()
            raise

    def reset(self) -> None:
        self._request_empty(Opcode.RESET)

    def unload(self) -> None:
        self._request_empty(Opcode.UNLOAD)

    def shutdown(self) -> None:
        if self._closed or self._shutdown:
            return
        deadline = self._transport.deadline()
        self._request_empty(Opcode.SHUTDOWN, deadline=deadline)
        self._shutdown = True
        returncode = self._transport.wait(deadline=deadline)
        if returncode != 0:
            detail = _diagnostic(self._transport.stderr())
            suffix = f": {detail}" if detail else ""
            raise VmProcessError(
                f"rv32vm serve exited with status {returncode}{suffix}"
            )
        self._transport.require_eof(deadline=deadline)

    def close(self) -> None:
        if self._closed:
            return
        try:
            if not self._shutdown:
                self.shutdown()
        finally:
            self._transport.close()
            self._closed = True

    def abort(self) -> None:
        self._transport.close()
        self._closed = True

    def __enter__(self) -> Self:
        return self

    def __exit__(
        self,
        exception_type: type[BaseException] | None,
        _exception: BaseException | None,
        _traceback: TracebackType | None,
    ) -> None:
        if exception_type is None:
            self.close()
        else:
            self.abort()
