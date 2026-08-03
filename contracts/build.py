"""Build and verify the generated VM contract artifacts."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

from elf_builder import build_elf, validate_elf

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parent.parent
HERE = ROOT / "contracts"
CASES = HERE / "cases.json"
ARTIFACTS = HERE / "artifacts"
MANIFEST = ARTIFACTS / "manifest.json"
TOOLCHAIN_LOCK = ROOT / "conformance/toolchain.env"
INSTALLED_TOOLCHAIN_LOCK = Path("/opt/conformance-toolchain.env")
DEFAULT_INSTRUCTION_LIMIT = 100_000_000
MAX_INSTRUCTION_LIMIT = 1_000_000_000

GCC = "riscv64-unknown-elf-gcc"
OBJCOPY = "riscv64-unknown-elf-objcopy"
NM = "riscv64-unknown-elf-nm"
TOOLCHAIN_KEYS = (
    "UBUNTU_IMAGE",
    "UBUNTU_SNAPSHOT",
    "RISCV_GNU_TOOLCHAIN_COMMIT",
    "RISCV_GCC_VERSION",
    "RISCV_BINUTILS_VERSION",
)
TRAP_CAUSES = {
    "InstructionAddressMisaligned",
    "InstructionAccessFault",
    "IllegalInstruction",
    "Breakpoint",
    "LoadAddressMisaligned",
    "LoadAccessFault",
    "StoreAddressMisaligned",
    "StoreAccessFault",
    "InvalidSyscall",
    "OutputLimitExceeded",
}


def _run(*arguments: str | Path, **options: object) -> subprocess.CompletedProcess:
    return subprocess.run(
        [str(argument) for argument in arguments],
        check=True,
        **options,
    )


def _output(*arguments: str | Path) -> str:
    return subprocess.check_output(
        [str(argument) for argument in arguments],
        text=True,
    ).strip()


def _sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _sha256(path: Path) -> str:
    return _sha256_bytes(path.read_bytes())


def _relative(path: Path) -> str:
    return path.relative_to(ROOT).as_posix()


def _read_lock(path: Path) -> dict[str, str]:
    values = {}
    for line in path.read_text().splitlines():
        if not line or line.startswith("#"):
            continue
        key, separator, value = line.partition("=")
        if not separator or not key or not value:
            raise RuntimeError(f"invalid toolchain lock line: {line!r}")
        values[key] = value
    return values


def _uint(value: object, maximum: int) -> bool:
    return (
        isinstance(value, int) and not isinstance(value, bool) and 0 <= value <= maximum
    )


def _hex(value: object, maximum_bytes: int) -> bool:
    if not isinstance(value, str) or len(value) % 2 or len(value) > maximum_bytes * 2:
        return False
    try:
        bytes.fromhex(value)
    except ValueError:
        return False
    return True


def _address(value: object) -> bool:
    return _uint(value, 0xFFFF_FFFF) or (isinstance(value, str) and bool(value))


def _validate_result(case_id: str, result: object) -> None:
    if not isinstance(result, dict):
        raise TypeError(f"{case_id}: result must be an object")
    status = result.get("status")
    common = {"status", "retired_instructions"}
    variants = {
        "exit": {"exit_code"},
        "trap": {"trap"},
        "resource_failure": {"resource_failure"},
    }
    if (
        not isinstance(status, str)
        or status not in variants
        or set(result) != common | variants[status]
        or not _uint(result.get("retired_instructions"), 0xFFFF_FFFF_FFFF_FFFF)
    ):
        raise RuntimeError(f"{case_id}: invalid expected result")
    if status == "exit" and not _uint(result["exit_code"], 0xFFFF_FFFF):
        raise RuntimeError(f"{case_id}: invalid exit code")
    if (
        status == "resource_failure"
        and result["resource_failure"] != "InstructionLimit"
    ):
        raise RuntimeError(f"{case_id}: invalid resource failure")
    if status == "trap":
        trap = result["trap"]
        if (
            not isinstance(trap, dict)
            or set(trap) != {"cause", "pc", "value"}
            or trap["cause"] not in TRAP_CAUSES
            or not _address(trap["pc"])
            or not _uint(trap["value"], 0xFFFF_FFFF)
        ):
            raise RuntimeError(f"{case_id}: invalid expected trap")


def _validate_state(case_id: str, state: object) -> None:
    if not isinstance(state, dict) or not set(state) <= {
        "pc",
        "registers",
        "memory",
    }:
        raise TypeError(f"{case_id}: invalid expected state")
    if "pc" in state and not _address(state["pc"]):
        raise RuntimeError(f"{case_id}: invalid expected pc")
    registers = state.get("registers")
    if isinstance(registers, list):
        if len(registers) != 32 or not all(
            _uint(value, 0xFFFF_FFFF) for value in registers
        ):
            raise RuntimeError(f"{case_id}: invalid register array")
    elif isinstance(registers, dict):
        if not all(
            isinstance(index, str)
            and index.isdecimal()
            and 0 <= int(index) < 32
            and _uint(value, 0xFFFF_FFFF)
            for index, value in registers.items()
        ):
            raise RuntimeError(f"{case_id}: invalid register mapping")
    elif registers is not None:
        raise TypeError(f"{case_id}: registers must be an array or object")

    memory = state.get("memory", [])
    if not isinstance(memory, list):
        raise TypeError(f"{case_id}: memory must be an array")
    total = 0
    for item in memory:
        if (
            not isinstance(item, dict)
            or set(item) != {"address", "data_hex"}
            or not _uint(item["address"], 0x0400_0000)
            or not _hex(item["data_hex"], 8 * 1024 * 1024)
        ):
            raise RuntimeError(f"{case_id}: invalid expected memory range")
        length = len(item["data_hex"]) // 2
        if item["address"] + length > 0x0400_0000:
            raise RuntimeError(f"{case_id}: expected memory range is out of bounds")
        total += length
    if len(memory) > 1024 or total > 8 * 1024 * 1024:
        raise RuntimeError(f"{case_id}: expected memory inspections exceed limits")


def _validate_runs(case_id: str, runs: list[object]) -> None:
    for run in runs:
        if not isinstance(run, dict) or not set(run) <= {
            "input_hex",
            "output_hex",
            "instruction_limit",
            "output_limit",
            "repeat",
            "result",
            "state",
        }:
            raise TypeError(f"{case_id}: invalid run object")
        if (
            not _hex(run.get("input_hex", ""), 4 * 1024 * 1024)
            or not _hex(run.get("output_hex", ""), 1024 * 1024)
            or not _uint(
                run.get("instruction_limit", DEFAULT_INSTRUCTION_LIMIT),
                MAX_INSTRUCTION_LIMIT,
            )
            or not _uint(run.get("output_limit", 1024 * 1024), 1024 * 1024)
            or not isinstance(run.get("repeat", 1), int)
            or isinstance(run.get("repeat", 1), bool)
            or run.get("repeat", 1) < 1
        ):
            raise RuntimeError(f"{case_id}: invalid run input or limit")
        if len(run.get("output_hex", "")) // 2 > run.get("output_limit", 1024 * 1024):
            raise RuntimeError(f"{case_id}: expected output exceeds its run limit")
        _validate_result(case_id, run.get("result"))
        if "state" in run:
            _validate_state(case_id, run["state"])


def _load_cases() -> list[dict[str, Any]]:
    try:
        document = json.loads(CASES.read_text())
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeError(f"cannot read {CASES}: {error}") from error
    if (
        not isinstance(document, dict)
        or set(document) != {"schema_version", "cases"}
        or document["schema_version"] != 1
        or not isinstance(document["cases"], list)
        or not document["cases"]
    ):
        raise RuntimeError("cases.json schema is invalid")

    seen = set()
    for case in document["cases"]:
        if not isinstance(case, dict):
            raise TypeError("cases.json contains a non-object case")
        case_id = case.get("id")
        kind = case.get("kind")
        specs = case.get("spec")
        if (
            not isinstance(case_id, str)
            or not case_id
            or "/" in case_id
            or case_id in seen
            or kind not in {"execute", "reject"}
            or not isinstance(specs, list)
            or not specs
            or not all(isinstance(spec, str) and spec for spec in specs)
        ):
            raise RuntimeError(f"invalid case record: {case_id!r}")
        seen.add(case_id)

        if kind == "execute":
            source = case.get("source")
            runs = case.get("runs")
            if (
                not set(case)
                <= {
                    "id",
                    "spec",
                    "kind",
                    "source",
                    "defines",
                    "elf_variant",
                    "runs",
                }
                or not isinstance(source, str)
                or not source.startswith("guest/")
                or not isinstance(runs, list)
                or not runs
            ):
                raise RuntimeError(f"invalid executable case: {case_id}")
            source_path = (HERE / source).resolve()
            if (
                not source_path.is_relative_to(HERE / "guest")
                or not source_path.is_file()
            ):
                raise RuntimeError(f"invalid source for {case_id}: {source}")
            defines = case.get("defines", [])
            if not isinstance(defines, list) or not all(
                isinstance(value, str) and value for value in defines
            ):
                raise RuntimeError(f"invalid defines for {case_id}")
            if "elf_variant" in case and not isinstance(case["elf_variant"], str):
                raise RuntimeError(f"invalid ELF variant for {case_id}")
            _validate_runs(case_id, runs)
        elif (
            set(case) != {"id", "spec", "kind", "elf_variant", "expected_rejection"}
            or not isinstance(case.get("elf_variant"), str)
            or not isinstance(case.get("expected_rejection"), str)
            or "runs" in case
        ):
            raise RuntimeError(f"invalid rejected-ELF case: {case_id}")
    return document["cases"]


def _check_tools() -> dict[str, str]:
    expected = _read_lock(TOOLCHAIN_LOCK)
    installed = _read_lock(INSTALLED_TOOLCHAIN_LOCK)
    if installed != expected:
        raise RuntimeError("container and repository toolchain locks differ")
    if sys.platform != "linux" or platform.machine() != "x86_64":
        raise RuntimeError("the canonical builder must run on linux/amd64")
    if _output(GCC, "-dumpfullversion") != expected["RISCV_GCC_VERSION"]:
        raise RuntimeError("the installed RISC-V GCC version is not pinned")
    assembler = _output("riscv64-unknown-elf-as", "--version").splitlines()[0]
    if not assembler.endswith(f" {expected['RISCV_BINUTILS_VERSION']}"):
        raise RuntimeError("the installed RISC-V binutils version is not pinned")
    return {key: expected[key] for key in TOOLCHAIN_KEYS}


def _toolchain() -> dict[str, str]:
    lock = _read_lock(TOOLCHAIN_LOCK)
    return {key: lock[key] for key in TOOLCHAIN_KEYS}


def _compile(
    case: dict[str, Any],
    work: Path,
) -> tuple[bytes, dict[str, int]]:
    case_id = case["id"]
    source = HERE / case["source"]
    object_path = work / f"{case_id}.o"
    linked_path = work / f"{case_id}.linked.elf"
    code_path = work / f"{case_id}.bin"
    definitions = [
        argument for value in case.get("defines", []) for argument in ("-D", value)
    ]
    _run(
        GCC,
        "-march=rv32im",
        "-mabi=ilp32",
        "-x",
        "assembler-with-cpp",
        *definitions,
        "-c",
        source,
        "-o",
        object_path,
    )
    _run(
        GCC,
        "-march=rv32im",
        "-mabi=ilp32",
        "-nostdlib",
        "-nostartfiles",
        "-static",
        "-Wl,--no-relax",
        "-Wl,--build-id=none",
        "-T",
        HERE / "guest/link.ld",
        object_path,
        "-o",
        linked_path,
    )
    _run(OBJCOPY, "-O", "binary", "-j", ".text", linked_path, code_path)

    symbols = {}
    for line in _output(
        NM, "--defined-only", "--format=posix", linked_path
    ).splitlines():
        fields = line.split()
        if len(fields) >= 3:
            symbols[fields[0]] = int(fields[2], 16)
    return code_path.read_bytes(), symbols


def _require_symbols(case: dict[str, Any], symbols: dict[str, int]) -> None:
    references = []
    for run in case["runs"]:
        result = run["result"]
        if result["status"] == "trap":
            references.append(result["trap"]["pc"])
        state = run.get("state", {})
        if "pc" in state:
            references.append(state["pc"])
    missing = {
        value for value in references if isinstance(value, str) and value not in symbols
    }
    if missing:
        raise RuntimeError(
            f"{case['id']}: unknown symbols: {', '.join(sorted(missing))}"
        )


def _inputs() -> dict[str, str]:
    paths = [
        HERE / "build.py",
        HERE / "cases.json",
        HERE / "elf_builder.py",
        ROOT / "conformance/Dockerfile",
        TOOLCHAIN_LOCK,
        *sorted((HERE / "guest").glob("*.S")),
        HERE / "guest/link.ld",
    ]
    return {_relative(path): _sha256(path) for path in paths}


def _manifest_bytes(document: dict[str, Any]) -> bytes:
    return (json.dumps(document, indent=2) + "\n").encode()


def _build(work: Path) -> dict[str, Any]:
    lock = _check_tools()
    cases = _load_cases()
    elf_root = work / "elf"
    elf_root.mkdir(parents=True)

    base_case = {
        "id": "_rejected-elf-base",
        "source": "guest/exit.S",
        "defines": ["EXIT_CODE=0"],
    }
    base_code, _ = _compile(base_case, work)
    records = []
    for case in cases:
        if case["kind"] == "execute":
            code, symbols = _compile(case, work)
            _require_symbols(case, symbols)
        else:
            code, symbols = base_code, {}
        variant = case.get("elf_variant", "default")
        elf = build_elf(code, variant)
        violation = validate_elf(elf)
        expected = case.get("expected_rejection")
        if violation != expected:
            raise RuntimeError(
                f"{case['id']}: expected ELF validation {expected!r}, got {violation!r}"
            )
        elf_name = f"{case['id']}.elf"
        (elf_root / elf_name).write_bytes(elf)
        records.append(
            {
                **case,
                "symbols": symbols,
                "elf": f"contracts/artifacts/elf/{elf_name}",
                "elf_sha256": _sha256_bytes(elf),
            }
        )
    return {
        "schema_version": 1,
        "builder_platform": "linux/amd64",
        "toolchain": lock,
        "project_inputs": _inputs(),
        "cases": records,
    }


def _publish(work: Path, manifest: dict[str, Any]) -> None:
    destination = ARTIFACTS / "elf"
    destination.mkdir(parents=True, exist_ok=True)
    expected = set()
    for source in (work / "elf").glob("*.elf"):
        target = destination / source.name
        expected.add(target)
        temporary = target.with_name(f".{target.name}.tmp")
        shutil.copyfile(source, temporary)
        os.replace(temporary, target)
    for stale in destination.glob("*.elf"):
        if stale not in expected:
            stale.unlink()

    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    temporary = MANIFEST.with_name(".manifest.json.tmp")
    temporary.write_bytes(_manifest_bytes(manifest))
    os.replace(temporary, MANIFEST)


def _check() -> dict[str, Any]:
    cases = _load_cases()
    try:
        manifest = json.loads(MANIFEST.read_text())
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeError(f"cannot read {MANIFEST}: {error}") from error
    if (
        not isinstance(manifest, dict)
        or manifest.get("schema_version") != 1
        or manifest.get("builder_platform") != "linux/amd64"
        or manifest.get("toolchain") != _toolchain()
        or manifest.get("project_inputs") != _inputs()
        or not isinstance(manifest.get("cases"), list)
        or len(manifest["cases"]) != len(cases)
    ):
        raise RuntimeError("generated manifest is stale or invalid")

    expected_artifacts = set()
    for source, record in zip(cases, manifest["cases"], strict=True):
        if not isinstance(record, dict):
            raise TypeError("generated manifest contains an invalid case")
        authored = {
            key: value
            for key, value in record.items()
            if key not in {"symbols", "elf", "elf_sha256"}
        }
        if authored != source:
            raise RuntimeError(f"manifest case is stale: {source['id']}")
        elf_name = record.get("elf")
        expected_hash = record.get("elf_sha256")
        symbols = record.get("symbols")
        if (
            not isinstance(elf_name, str)
            or not isinstance(expected_hash, str)
            or not isinstance(symbols, dict)
            or not all(
                isinstance(name, str)
                and isinstance(address, int)
                and 0 <= address <= 0xFFFF_FFFF
                for name, address in symbols.items()
            )
        ):
            raise RuntimeError(f"invalid generated fields: {source['id']}")
        elf_path = (ROOT / elf_name).resolve()
        expected_path = (ARTIFACTS / "elf" / f"{source['id']}.elf").resolve()
        if elf_path != expected_path or not elf_path.is_file():
            raise RuntimeError(f"missing ELF artifact: {elf_name}")
        expected_artifacts.add(elf_path)
        elf = elf_path.read_bytes()
        if _sha256_bytes(elf) != expected_hash:
            raise RuntimeError(f"ELF hash mismatch: {elf_name}")
        if validate_elf(elf) != source.get("expected_rejection"):
            raise RuntimeError(f"ELF validation mismatch: {source['id']}")
    actual_artifacts = {
        path.resolve() for path in (ARTIFACTS / "elf").rglob("*") if path.is_file()
    }
    if actual_artifacts != expected_artifacts:
        raise RuntimeError("generated ELF artifact inventory is stale")
    return manifest


def build() -> None:
    with tempfile.TemporaryDirectory(prefix="rv32im-contract-build-") as temporary:
        work = Path(temporary)
        manifest = _build(work)
        _publish(work, manifest)
    print(f"built {len(manifest['cases'])} contract ELFs")


def check() -> None:
    manifest = _check()
    print(f"verified {len(manifest['cases'])} contract ELFs and manifest")


def reproduce() -> None:
    existing = _manifest_bytes(_check())
    with tempfile.TemporaryDirectory(prefix="rv32im-contract-reproduce-") as temporary:
        work = Path(temporary)
        manifest = _build(work)
        if _manifest_bytes(manifest) != existing:
            raise RuntimeError("contract manifest is not reproducible")
        for record in manifest["cases"]:
            generated = work / "elf" / f"{record['id']}.elf"
            committed = ROOT / record["elf"]
            if generated.read_bytes() != committed.read_bytes():
                raise RuntimeError(f"ELF is not reproducible: {record['id']}")
    print(f"reproduced all {len(manifest['cases'])} contract ELFs")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("build", "check", "reproduce"))
    arguments = parser.parse_args()
    try:
        globals()[arguments.command]()
    except (OSError, RuntimeError, TypeError, subprocess.CalledProcessError) as error:
        print(f"contract artifact {arguments.command} failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
