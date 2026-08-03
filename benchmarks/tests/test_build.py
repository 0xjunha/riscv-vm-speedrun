from __future__ import annotations

import json
import shutil
import struct
import sys
from pathlib import Path

import pytest

BENCHMARKS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(BENCHMARKS))

import build
import reference


def test_checked_in_artifacts() -> None:
    build.check()
    build.check_long()


def test_reference_records_are_stable() -> None:
    expected = {
        "tiny": "525642310100000024bb34a108000000",
        "arithmetic": "5256423103000000b45a03028dac6b0e",
        "streaming": "52564231050000001eb9132b00d8beff",
        "sha256": "52564231070000002b7fa3a8f050c784",
        "heatshrink": "5256423109000000a599a52541de76c0",
        "depthconv": "525642310b0000003e48b268c2e8ec02",
        "dijkstra": "525642310d0000002ec00100de5dd2b4",
        "sort_records": "525642310f0000001f22b550eb0131eb",
        "qrcode": "5256423111000000a80400007d9fde3e",
    }
    for case in build.load_cases():
        data = reference.input_for(case.workload, case.parameters)
        assert reference.output_for(case.workload, data).hex() == expected[case.id]


def test_manifest_has_no_scoring_or_hidden_fields() -> None:
    manifest = (build.ARTIFACTS / "manifest.json").read_text(encoding="utf-8")
    for excluded in ("hidden", "reward", "score", "threshold"):
        assert excluded not in manifest


def test_builder_metadata_comes_from_dockerfile(tmp_path: Path) -> None:
    base_image = "rust:1.96.1-slim-bookworm@sha256:" + "a" * 64
    dockerfile = tmp_path / "Dockerfile"
    dockerfile.write_text(f"FROM {base_image}\n", encoding="utf-8")

    metadata = build._builder_metadata(dockerfile)
    assert metadata["base_image"] == base_image
    assert "image" not in metadata


def test_guest_source_checks_use_strict_targeted_commands(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls = []

    def fake_run(command: list[str], **options: object) -> None:
        calls.append((command, options))

    monkeypatch.setattr(build.subprocess, "run", fake_run)

    build._check_guest_sources(tmp_path)

    assert [command for command, _ in calls] == [
        [
            "cargo",
            "fmt",
            "--package",
            "rv32im-guest",
            "--package",
            "rv32im-workloads",
            "--",
            "--check",
        ],
        [
            "cargo",
            "clippy",
            "--frozen",
            "--workspace",
            "--lib",
            "--bins",
            "--all-features",
            "--release",
            "--target",
            build.TARGET,
            "--",
            "-D",
            "warnings",
            "-D",
            "clippy::all",
        ],
        [
            "cargo",
            "clippy",
            "--frozen",
            "--package",
            "rv32im-workloads",
            "--lib",
            "--bins",
            "--release",
            "--target",
            build.NATIVE_TARGET,
            "--",
            "-D",
            "warnings",
            "-D",
            "clippy::all",
        ],
        [
            "cargo",
            "test",
            "--frozen",
            "--package",
            "rv32im-workloads",
            "--lib",
            "--all-features",
            "--target",
            build.NATIVE_TARGET,
        ],
    ]
    for _, options in calls:
        assert options["cwd"] == build.GUEST
        assert options["check"] is True
        assert options["env"]["CARGO_TARGET_DIR"] == str(tmp_path)
        assert options["env"]["CARGO_NET_OFFLINE"] == "true"


def test_long_cases_require_configured_case_ids() -> None:
    case_id = build.load_long_suite().case_ids[0]
    cases = tuple(case for case in build.load_cases() if case.id != case_id)
    with pytest.raises(build.BuildError, match=f"missing long cases: {case_id}"):
        build._long_cases(cases)


@pytest.mark.parametrize(
    ("field", "values", "message"),
    [
        ("horizons", [10, 10], "duplicate long horizon"),
        ("case_ids", ["sha256", "sha256"], "duplicate long case id"),
    ],
)
def test_long_suite_rejects_duplicates(
    tmp_path: Path, field: str, values: list[object], message: str
) -> None:
    document = {
        "schema_version": 1,
        "horizons": [10, 100],
        "case_ids": ["sha256"],
    }
    document[field] = values
    path = tmp_path / "long_cases.json"
    path.write_text(json.dumps(document), encoding="utf-8")

    with pytest.raises(build.BuildError, match=message):
        build.load_long_suite(path)


def test_only_long_manifest_tracks_long_suite_config() -> None:
    path = build.ROOT / "long_cases.json"
    assert path not in build._project_input_paths()
    assert path in build._project_input_paths(long=True)


@pytest.mark.parametrize(
    "contents",
    [
        "FROM rust:1.96.1-slim-bookworm\n",
        "FROM rust:1.96.1-slim-bookworm@sha256:short\n",
        "FROM ubuntu:24.04@sha256:" + "a" * 64 + "\n",
        (
            "FROM rust:1.96.1-slim-bookworm@sha256:"
            + "a" * 64
            + "\nFROM rust:1.96.1-slim-bookworm@sha256:"
            + "b" * 64
            + "\n"
        ),
    ],
)
def test_builder_rejects_unpinned_or_malformed_from(
    tmp_path: Path, contents: str
) -> None:
    dockerfile = tmp_path / "Dockerfile"
    dockerfile.write_text(contents, encoding="utf-8")

    with pytest.raises(build.BuildError):
        build._builder_metadata(dockerfile)


def test_cases_reject_unknown_fields(tmp_path: Path) -> None:
    document = json.loads((build.ROOT / "cases.json").read_text(encoding="utf-8"))
    document["cases"][0]["unknown"] = True
    path = tmp_path / "cases.json"
    path.write_text(json.dumps(document), encoding="utf-8")

    with pytest.raises(build.BuildError, match="fields"):
        build.load_cases(path)


def test_heatshrink_limit_is_checked_before_generation(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def fail_if_called(records: int, seed: int) -> bytes:
        raise AssertionError(f"generated {records=} with {seed=}")

    monkeypatch.setattr(reference, "_telemetry", fail_if_called)
    records = reference.HEATSHRINK_MAX_PAYLOAD // reference.TELEMETRY_RECORD_SIZE + 1

    with pytest.raises(ValueError, match="16 KiB"):
        reference.input_for("heatshrink", {"records": records, "seed": 0})


def test_third_party_inputs_exclude_standalone_cargo_state(tmp_path: Path) -> None:
    crate = tmp_path / "dependency"
    source = crate / "src/lib.rs"
    generated = crate / "target/debug/build/generated.rs"
    for path in (crate / "Cargo.toml", source, crate / "Cargo.lock", generated):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("")

    assert build._third_party_input_paths(tmp_path) == (
        crate / "Cargo.toml",
        source,
    )


def test_check_rejects_extra_artifacts(tmp_path: Path) -> None:
    artifacts = tmp_path / "artifacts"
    shutil.copytree(build.ARTIFACTS, artifacts)
    (artifacts / "extra.bin").write_bytes(b"extra")

    with pytest.raises(build.BuildError, match="extra=.*extra.bin"):
        build.check(artifacts)


def test_elf_validation_rejects_unsupported_flags(tmp_path: Path) -> None:
    data = bytearray((build.ARTIFACTS / "elf/tiny.elf").read_bytes())
    struct.pack_into("<I", data, 36, 1)
    path = tmp_path / "bad-flags.elf"
    path.write_bytes(data)

    with pytest.raises(build.BuildError, match="flags"):
        build._validate_elf(path)


@pytest.mark.parametrize(
    ("offset", "encoding", "value", "message"),
    [
        (20, "<I", 0, "version"),
        (44, "<H", 0, "program-header"),
        (44, "<H", 0xFFFF, "program-header"),
        (48, "<H", 0, "extended section"),
        (50, "<H", 6, "section headers"),
    ],
)
def test_elf_validation_rejects_header_extensions(
    tmp_path: Path,
    offset: int,
    encoding: str,
    value: int,
    message: str,
) -> None:
    data = bytearray((build.ARTIFACTS / "elf/tiny.elf").read_bytes())
    struct.pack_into(encoding, data, offset, value)
    path = tmp_path / "bad-header.elf"
    path.write_bytes(data)

    with pytest.raises(build.BuildError, match=message):
        build._validate_elf(path)


@pytest.mark.parametrize("section_type", [4, 6, 9])
def test_elf_validation_rejects_allocated_dynamic_sections(
    tmp_path: Path, section_type: int
) -> None:
    data = bytearray((build.ARTIFACTS / "elf/tiny.elf").read_bytes())
    header = struct.unpack_from("<16sHHIIIIIHHHHHH", data)
    section = header[6] + header[11]
    struct.pack_into("<I", data, section + 4, section_type)
    path = tmp_path / "dynamic.elf"
    path.write_bytes(data)

    with pytest.raises(build.BuildError, match="dynamic or relocation"):
        build._validate_elf(path)


def test_elf_validation_rejects_tls_section(tmp_path: Path) -> None:
    data = bytearray((build.ARTIFACTS / "elf/tiny.elf").read_bytes())
    header = struct.unpack_from("<16sHHIIIIIHHHHHH", data)
    section_flags = header[6] + header[11] + 8
    current = struct.unpack_from("<I", data, section_flags)[0]
    struct.pack_into("<I", data, section_flags, current | 0x400)
    path = tmp_path / "tls.elf"
    path.write_bytes(data)

    with pytest.raises(build.BuildError, match="TLS"):
        build._validate_elf(path)


def test_elf_validation_requires_nonempty_load(tmp_path: Path) -> None:
    data = bytearray((build.ARTIFACTS / "elf/tiny.elf").read_bytes())
    header = struct.unpack_from("<16sHHIIIIIHHHHHH", data)
    load = header[5]
    struct.pack_into("<II", data, load + 16, 0, 0)
    path = tmp_path / "empty-load.elf"
    path.write_bytes(data)

    with pytest.raises(build.BuildError, match="nonempty"):
        build._validate_elf(path)


def test_elf_validation_rejects_nonword_executable_memory(tmp_path: Path) -> None:
    data = bytearray((build.ARTIFACTS / "elf/tiny.elf").read_bytes())
    header = struct.unpack_from("<16sHHIIIIIHHHHHH", data)
    load_memory_size = header[5] + 20
    current = struct.unpack_from("<I", data, load_memory_size)[0]
    struct.pack_into("<I", data, load_memory_size, current + 1)
    path = tmp_path / "unaligned-executable.elf"
    path.write_bytes(data)

    with pytest.raises(build.BuildError, match="word-aligned"):
        build._validate_elf(path)
