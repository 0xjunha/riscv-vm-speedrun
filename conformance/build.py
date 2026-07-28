"""Verify sources and build the 95 public RV32IM conformance ELFs."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import struct
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parent.parent
HERE = ROOT / "conformance"
ACT4 = ROOT / "third_party/riscv/conformance/upstream" / "riscv-arch-test-act4-619aa169"
RISCV_TESTS = ROOT / "third_party/riscv/conformance/upstream" / "riscv-tests-34e6b6d1"
ARTIFACTS = HERE / "artifacts"
ELF_ARTIFACTS = ARTIFACTS / "elf"
ACT4_REFERENCE_RESULTS = ARTIFACTS / "reference-results/act4"
MANIFEST = ARTIFACTS / "manifest.json"
GCC = "riscv64-unknown-elf-gcc"

RISCV_TESTS_I = (
    "simple",
    "add",
    "addi",
    "and",
    "andi",
    "auipc",
    "beq",
    "bge",
    "bgeu",
    "blt",
    "bltu",
    "bne",
    "jal",
    "jalr",
    "lb",
    "lbu",
    "lh",
    "lhu",
    "lw",
    "ld_st",
    "lui",
    "or",
    "ori",
    "sb",
    "sh",
    "sw",
    "st_ld",
    "sll",
    "slli",
    "slt",
    "slti",
    "sltiu",
    "sltu",
    "sra",
    "srai",
    "srl",
    "srli",
    "sub",
    "xor",
    "xori",
)
RISCV_TESTS_M = ("div", "divu", "mul", "mulh", "mulhsu", "mulhu", "rem", "remu")


@dataclass(frozen=True)
class Case:
    suite: str
    case_id: str
    isa: str
    source: Path
    act4_extension: str | None = None


def elf_path(root: Path, case: Case) -> Path:
    return root / case.suite / f"{case.case_id}.elf"


def act4_result_path(root: Path, case: Case) -> Path:
    if case.act4_extension is None:
        raise RuntimeError(f"ACT4 case has no extension: {case.case_id}")
    return root / case.act4_extension / f"{case.case_id}.results"


def run(*args: str | Path, **kwargs: object) -> subprocess.CompletedProcess:
    return subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def output(*args: str | Path) -> str:
    return subprocess.check_output([str(arg) for arg in args], text=True).strip()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def relative(path: Path) -> str:
    return path.relative_to(ROOT).as_posix()


def read_lock(path: Path = HERE / "toolchain.env") -> dict[str, str]:
    values: dict[str, str] = {}
    for line in path.read_text().splitlines():
        if line and not line.startswith("#"):
            key, separator, value = line.partition("=")
            if not separator or not key or not value:
                raise RuntimeError(f"invalid toolchain lock line: {line!r}")
            values[key] = value
    return values


def verify_imported_sources() -> None:
    run(ROOT / "scripts/verify-riscv-conformance.sh")


def act4_cases() -> list[Case]:
    cases = []
    for extension in ("I", "M"):
        isa = "rv32i" if extension == "I" else "rv32im"
        for source in sorted((ACT4 / "tests/rv32i" / extension).glob("*.S")):
            cases.append(Case("act4", source.stem, isa, source, extension))
    if len(cases) != 47:
        raise RuntimeError(f"expected 47 ACT4 sources, found {len(cases)}")
    return cases


def riscv_test_cases() -> list[Case]:
    cases = [
        Case(
            "riscv-tests",
            f"rv32ui-p-{name}",
            "rv32i",
            RISCV_TESTS / "isa/rv32ui" / f"{name}.S",
        )
        for name in RISCV_TESTS_I
    ]
    cases += [
        Case(
            "riscv-tests",
            f"rv32um-p-{name}",
            "rv32im",
            RISCV_TESTS / "isa/rv32um" / f"{name}.S",
        )
        for name in RISCV_TESTS_M
    ]
    if len(cases) != 48 or any(not case.source.is_file() for case in cases):
        raise RuntimeError("the expected 48 riscv-tests sources are not present")
    return cases


def verify_act4_generator(destination: Path) -> None:
    """Regenerate ACT4 I/M assembly and require byte-for-byte identity."""
    sys.path.insert(0, str(ACT4 / "generators/testgen/src"))
    from testgen.generate.unpriv import generate_unpriv_extension_tests

    for extension in ("I", "M"):
        generate_unpriv_extension_tests(
            xlen=32,
            E_ext=False,
            testsuite=extension,
            testplan_dir=ACT4 / "testplans",
            output_test_dir=destination,
        )

    generated_root = destination / "rv32i"
    imported_root = ACT4 / "tests/rv32i"
    generated = sorted(
        path.relative_to(generated_root)
        for extension in ("I", "M")
        for path in (generated_root / extension).glob("*.S")
    )
    imported = sorted(case.source.relative_to(imported_root) for case in act4_cases())
    if generated != imported:
        raise RuntimeError("ACT4 generated source inventory differs from the import")
    for path in imported:
        if (generated_root / path).read_bytes() != (imported_root / path).read_bytes():
            raise RuntimeError(f"ACT4 generator output differs: {path}")


def check_sources(work: Path, *, verify_snapshot: bool = True) -> None:
    if verify_snapshot:
        verify_imported_sources()
    verify_act4_generator(work / "generated")
    riscv_test_cases()
    print("verified imported sources and all 95 selected cases")


def prepare_act4_environment(work: Path) -> Path:
    env = work / "tests/env"
    shutil.copytree(ACT4 / "tests/env", env)
    for patch in sorted((HERE / "patches").glob("*.patch")):
        run("patch", "--silent", "-d", work, "-p1", "-i", patch)
    shutil.copyfile(HERE / "adapters/act4/rvtest_config.h", env / "rvtest_config.h")
    shutil.copyfile(HERE / "adapters/act4/rvmodel_macros.h", env / "rvmodel_macros.h")
    return env


def derive_sail_config(destination: Path) -> None:
    """Narrow Sail's pinned stock RV32 profile to the project's RV32IM EEI."""
    sail_root = Path("/opt/sail/share/sail-riscv")
    source = sail_root / "config/rv32d_v128_e32.json"
    text = re.sub(r"//.*$", "", source.read_text(), flags=re.MULTILINE)
    text = re.sub(r",\s*([}\]])", r"\1", text)
    config = json.loads(text)
    config["$schema"] = str(sail_root / "sail_riscv_config_schema.json")

    base = config["base"]
    for key in (
        "mcounteren_writable_bits",
        "scounteren_writable_bits",
        "writable_hpm_counters",
    ):
        base[key]["value"] = "0x0"
    base["mstatus"]["fs_legal_states"] = "ExtContext_Off"
    base["mstatus"]["vs_legal_states"] = "ExtContext_Off"
    base["reserved_behavior"].update(
        amocas_odd_register="AMOCAS_Fatal",
        fcsr_rm="Fcsr_RM_Fatal",
        rv32zdinx_odd_register="Zdinx_Fatal",
    )
    base["stvec"]["direct"]["supported"] = False
    base["stvec"]["vectored"]["supported"] = False
    base["writable_fiom"] = False
    base["writable_misa"] = False

    memory = config["memory"]
    memory["pmp"]["count"] = 0
    memory["pmp"]["usable_count"] = 0
    for access in ("load_store", "vector"):
        memory["misaligned"]["exceptions"][access] = {"Some": "AlignmentException"}
    rom, mmio, ram = memory["regions"]
    ram["base"]["value"] = "0x10000"
    ram["size"]["value"] = "0x2ff0000"
    ram["attributes"].update(
        atomic_support="AMONone",
        reservability="RsrvNone",
        supports_cbo_zero=False,
        supports_pte_read=False,
        supports_pte_write=False,
    )
    mmio["base"]["value"] = "0x10000000"
    mmio["size"]["value"] = "0x1000"
    memory["regions"] = [rom, ram, mmio]

    device = config["platform"]
    device["clint"].update(supported=False, base=0, size=0)
    device["simple_interrupt_generator"].update(supported=False, base=0)
    device["max_time_to_wait"] = 200
    device["reservation"].update(
        require_exact_reservation_addr=True,
        reservation_set_size_exp=2,
    )

    def disable_extensions(value: object) -> None:
        if isinstance(value, dict):
            if "supported" in value:
                value["supported"] = False
            for child in value.values():
                disable_extensions(child)
        elif isinstance(value, list):
            for child in value:
                disable_extensions(child)

    disable_extensions(config["extensions"])
    config["extensions"]["M"]["supported"] = True
    config["extensions"]["Zmmul"]["supported"] = True
    config["extensions"]["V"].update(
        elen_exp=6,
        max_index_eew_exp=6,
        support_level="Disabled",
        vlen_exp=8,
    )
    destination.write_text(json.dumps(config, indent=2) + "\n")


def check_tools() -> dict[str, str]:
    expected = read_lock()
    installed = read_lock(Path("/opt/conformance-toolchain.env"))
    if installed != expected:
        raise RuntimeError("container and repository toolchain locks differ")
    if sys.platform != "linux" or platform.machine() != "x86_64":
        raise RuntimeError("the canonical builder must run on linux/amd64")

    gcc = output(GCC, "-dumpfullversion")
    assembler = output("riscv64-unknown-elf-as", "--version").splitlines()[0]
    sail = output("sail_riscv_sim", "--version")
    if gcc != expected["RISCV_GCC_VERSION"]:
        raise RuntimeError(f"expected GCC {expected['RISCV_GCC_VERSION']}, got {gcc}")
    if not assembler.endswith(f" {expected['RISCV_BINUTILS_VERSION']}"):
        raise RuntimeError(f"unexpected assembler version: {assembler}")
    if sail != expected["SAIL_VERSION"]:
        raise RuntimeError(f"expected Sail {expected['SAIL_VERSION']}, got {sail}")
    return expected


def compile_object(
    case: Case, destination: Path, includes: list[Path], defines: list[str]
) -> None:
    run(
        GCC,
        f"-march={case.isa}",
        "-mabi=ilp32",
        "-O0",
        "-g0",
        "-mcmodel=medany",
        "-fno-pic",
        "-fno-pie",
        "-ffreestanding",
        f"-frandom-seed={case.case_id}",
        *(item for include in includes for item in ("-I", include)),
        *defines,
        "-c",
        case.source,
        "-o",
        destination,
    )


def link(case: Case, source: Path, script: Path, destination: Path) -> None:
    run(
        GCC,
        f"-march={case.isa}",
        "-mabi=ilp32",
        "-nostdlib",
        "-nostartfiles",
        "-static",
        "-Wl,--no-relax",
        "-Wl,--build-id=none",
        "-Wl,--no-warn-rwx-segments",
        "-T",
        script,
        source,
        "-o",
        destination,
    )
    data = destination.read_bytes()
    if (
        len(data) < 52
        or data[:6] != b"\x7fELF\x01\x01"
        or struct.unpack_from("<HH", data, 16) != (2, 243)
    ):
        raise RuntimeError(f"invalid ELF output: {destination.name}")
    run(
        "riscv64-unknown-elf-readelf",
        "-h",
        "-l",
        destination,
        stdout=subprocess.DEVNULL,
    )


def build_riscv_tests(work: Path, elf_root: Path) -> list[Case]:
    cases = riscv_test_cases()
    objects = work / "objects/riscv-tests"
    objects.mkdir(parents=True)
    (elf_root / "riscv-tests").mkdir(parents=True)
    includes = [
        HERE / "adapters/riscv-tests",
        RISCV_TESTS / "isa/macros/scalar",
        RISCV_TESTS / "isa",
    ]
    for case in cases:
        obj = objects / f"{case.case_id}.o"
        compile_object(case, obj, includes, [])
        link(
            case,
            obj,
            HERE / "adapters/riscv-tests/link.ld",
            elf_path(elf_root, case),
        )
    print("built 48 riscv-tests ELFs")
    return cases


def build_act4(work: Path, elf_root: Path, reference_root: Path) -> list[Case]:
    cases = act4_cases()
    env = prepare_act4_environment(work)
    objects = work / "objects/act4"
    objects.mkdir(parents=True)
    (elf_root / "act4").mkdir(parents=True)
    sail_config = work / "sail.json"
    derive_sail_config(sail_config)

    sys.path.insert(0, str(ACT4 / "framework/src"))
    from act.sig_modify import process_signature_file

    for case in cases:
        reference_object = objects / f"{case.case_id}.reference.o"
        reference_elf = objects / f"{case.case_id}.reference.elf"
        compile_object(
            case,
            reference_object,
            [env],
            ["-DSIGNATURE", "-DTEST_FLEN=32"],
        )
        link(case, reference_object, HERE / "adapters/act4/link.ld", reference_elf)

        sail_signature = objects / f"{case.case_id}.sig"
        sail_log = objects / f"{case.case_id}.sail.log"
        with sail_log.open("w") as log:
            run(
                "sail_riscv_sim",
                "--config",
                sail_config,
                f"--test-signature={sail_signature}",
                "--signature-granularity",
                "4",
                reference_elf,
                stdout=log,
                stderr=subprocess.STDOUT,
            )
        process_signature_file(sail_signature, 32)
        result = sail_signature.with_suffix(".results")
        final_result = act4_result_path(reference_root, case)
        final_result.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(result, final_result)

        final_obj = objects / f"{case.case_id}.o"
        compile_object(
            case,
            final_obj,
            [env],
            [
                "-DRVTEST_SELFCHECK",
                "-DXLEN=32",
                "-DTEST_FLEN=32",
                f'-DSIGNATURE_FILE="{final_result}"',
            ],
        )
        link(
            case,
            final_obj,
            HERE / "adapters/act4/link.ld",
            elf_path(elf_root, case),
        )
    print("generated 47 Sail reference results and built 47 ACT4 ELFs")
    return cases


def input_hashes() -> dict[str, str]:
    inputs = [HERE / "build.py", HERE / "Dockerfile", HERE / "toolchain.env"]
    inputs += sorted(path for path in (HERE / "adapters").rglob("*") if path.is_file())
    inputs += sorted((HERE / "patches").glob("*.patch"))
    return {relative(path): sha256(path) for path in inputs}


def make_manifest(
    lock: dict[str, str],
    cases: list[Case],
    elf_root: Path,
    reference_root: Path,
) -> dict[str, object]:
    records = []
    for case in sorted(cases, key=lambda item: (item.suite, item.case_id)):
        elf = elf_path(elf_root, case)
        record = {
            "suite": case.suite,
            "id": case.case_id,
            "isa": case.isa,
            "source": relative(case.source),
            "source_sha256": sha256(case.source),
            "elf": relative(elf_path(ELF_ARTIFACTS, case)),
            "elf_sha256": sha256(elf),
        }
        if case.act4_extension:
            result = act4_result_path(reference_root, case)
            record.update(
                reference_result=relative(
                    act4_result_path(ACT4_REFERENCE_RESULTS, case)
                ),
                reference_result_sha256=sha256(result),
            )
        records.append(record)
    return {
        "schema_version": 1,
        "builder_platform": "linux/amd64",
        "toolchain": lock,
        "project_inputs": input_hashes(),
        "cases": records,
    }


def manifest_bytes(manifest: dict[str, object]) -> bytes:
    return (json.dumps(manifest, indent=2) + "\n").encode()


def publish_file(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f".{destination.name}.tmp")
    shutil.copyfile(source, temporary)
    os.replace(temporary, destination)


def publish(work: Path, manifest: dict[str, object], cases: list[Case]) -> None:
    staged_elf = work / "elf"
    staged_results = work / "reference-results/act4"
    expected_elf = set()
    expected_results = set()
    for case in cases:
        elf = elf_path(ELF_ARTIFACTS, case)
        expected_elf.add(elf)
        publish_file(elf_path(staged_elf, case), elf)
        if case.act4_extension:
            result = act4_result_path(ACT4_REFERENCE_RESULTS, case)
            expected_results.add(result)
            publish_file(act4_result_path(staged_results, case), result)
    for stale in ELF_ARTIFACTS.glob("*/*.elf"):
        if stale not in expected_elf:
            stale.unlink()
    for stale in ACT4_REFERENCE_RESULTS.glob("*/*.results"):
        if stale not in expected_results:
            stale.unlink()

    MANIFEST.parent.mkdir(parents=True, exist_ok=True)
    temporary_manifest = MANIFEST.with_name(".manifest.json.tmp")
    temporary_manifest.write_bytes(manifest_bytes(manifest))
    os.replace(temporary_manifest, MANIFEST)


def verify_existing(work: Path, manifest: dict[str, object], cases: list[Case]) -> None:
    for case in cases:
        generated = elf_path(work / "elf", case)
        existing = elf_path(ELF_ARTIFACTS, case)
        if not existing.is_file() or generated.read_bytes() != existing.read_bytes():
            raise RuntimeError(f"ELF is not reproducible: {relative(existing)}")
        if case.act4_extension:
            generated = act4_result_path(work / "reference-results/act4", case)
            existing = act4_result_path(ACT4_REFERENCE_RESULTS, case)
            if (
                not existing.is_file()
                or generated.read_bytes() != existing.read_bytes()
            ):
                raise RuntimeError(
                    f"reference result is not reproducible: {relative(existing)}"
                )
    if not MANIFEST.is_file() or manifest_bytes(manifest) != MANIFEST.read_bytes():
        raise RuntimeError("manifest is not reproducible")
    print("reproduced all 95 ELFs and 47 ACT4 reference results")


def check_manifest() -> None:
    cases = act4_cases() + riscv_test_cases()
    expected = make_manifest(
        read_lock(),
        cases,
        ELF_ARTIFACTS,
        ACT4_REFERENCE_RESULTS,
    )
    if json.loads(MANIFEST.read_text()) != expected:
        raise RuntimeError("manifest differs from the current inputs and artifacts")
    print("verified manifest and generated artifact hashes")


def build(reproduce: bool) -> None:
    lock = check_tools()
    with tempfile.TemporaryDirectory(prefix="rv32im-conformance-") as temp:
        work = Path(temp)
        # The host verifies imported file modes. macOS bind mounts do not
        # preserve them accurately inside Linux containers.
        check_sources(work, verify_snapshot=False)
        elf_root = work / "elf"
        reference_root = work / "reference-results/act4"
        cases = build_riscv_tests(work, elf_root)
        cases += build_act4(work, elf_root, reference_root)
        manifest = make_manifest(lock, cases, elf_root, reference_root)
        if reproduce:
            verify_existing(work, manifest, cases)
        else:
            publish(work, manifest, cases)
            print("published 95 conformance ELFs")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "command",
        choices=("check-sources", "check", "build", "reproduce"),
    )
    command = parser.parse_args().command
    if command in ("build", "reproduce"):
        build(command == "reproduce")
        return
    with tempfile.TemporaryDirectory(prefix="rv32im-conformance-sources-") as temp:
        check_sources(Path(temp))
    if command == "check":
        check_manifest()


if __name__ == "__main__":
    main()
