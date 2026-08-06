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
from reference import aes as reference_aes
from reference import dijkstra as reference_dijkstra
from reference import heatshrink as reference_heatshrink
from reference import littlefs as reference_littlefs
from reference import qrcode as reference_qrcode
from reference import sha256 as reference_sha256
from reference import sort_records as reference_sort_records
from reference import streaming as reference_streaming
from reference import x25519 as reference_x25519


def test_checked_in_artifacts() -> None:
    build.check_all()


def test_aes256_reference_known_answer() -> None:
    key = bytes.fromhex(
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
    )
    plaintext = bytes.fromhex("00112233445566778899aabbccddeeff")
    assert reference_aes._encrypt_block(key, plaintext).hex() == (
        "8ea2b7ca516745bfeafc49904b496089"
    )


def test_aes_random_records_use_nonoverlapping_material() -> None:
    data = reference.input_for(
        "aes", {"records": 2, "profile": "random", "seed": 305_419_896}
    )
    chunks = tuple(data[offset : offset + 32] for offset in range(0, len(data), 32))

    assert len(chunks) == 4
    key_0, plaintext_0, key_1, plaintext_1 = chunks
    assert key_0[1:] != plaintext_0[:-1]
    assert plaintext_0[1:] != key_1[:-1]
    assert key_1[1:] != plaintext_1[:-1]


def test_sglib_random_profile_spans_signed_values() -> None:
    data = reference.input_for(
        "sglib", {"count": 384, "profile": "random", "seed": 826_366_246}
    )
    values = tuple(value[0] for value in struct.iter_unpack("<i", data))

    assert min(values) < 0 < max(values)
    assert max(values) - min(values) > 1 << 31


def test_application_case_profiles_are_diversified() -> None:
    cases = build.load_cases()
    counts = {
        workload: sum(case.workload == workload for case in cases)
        for workload in build.load_application_workloads()
    }
    assert counts == dict.fromkeys(counts, 3)


def test_structured_profiles_match_their_names() -> None:
    ordered_data = reference.input_for(
        "sglib", {"count": 384, "profile": "ordered", "seed": 2_576_980_377}
    )
    ordered = [value[0] for value in struct.iter_unpack("<i", ordered_data)]
    assert ordered == sorted(ordered)

    positive_data = reference.input_for(
        "ud", {"profile": "positive", "seed": 3_435_973_836, "systems": 1}
    )
    assert all(value[0] >= 0 for value in struct.iter_unpack("<i", positive_data))


def test_application_workloads_are_complete_and_consistent() -> None:
    application = set(build.load_application_workloads())
    assert application == set(build.WORKLOADS) - {"tiny", "arithmetic", "streaming"}


def test_authored_profiles_have_their_intended_shapes() -> None:
    repeated = reference_sha256.firmware_payload(768, 1, "repeated-pages")
    assert repeated[:256] == repeated[256:512] == repeated[512:]

    sparse_flash = reference_sha256.firmware_payload(1024, 2, "sparse-flash")
    assert sparse_flash.count(0xFF) >= 950

    ascending = struct.unpack("<16I", reference_sort_records.records(8, 3, "ascending"))
    assert ascending[::2] == tuple(range(8))
    duplicates = struct.unpack(
        "<128I", reference_sort_records.records(64, 4, "duplicate-heavy")
    )
    assert len(set(duplicates[::2])) <= 16

    hub = struct.unpack("<64H", reference_dijkstra.graph(8, 0, 5, "hub"))
    assert all(hub[target] for target in range(1, 8))
    assert all(hub[source * 8] for source in range(1, 8))

    mixed = struct.unpack("<80I", reference_littlefs.operations(20, 9, "mixed"))
    append = struct.unpack(
        "<64I", reference_littlefs.operations(16, 10, "append-heavy")
    )
    metadata = struct.unpack(
        "<96I", reference_littlefs.operations(24, 11, "metadata-churn")
    )
    assert mixed[::4] == (0, 1, 2, 3, 4) * 4
    assert append[::4] == (0, 1, 1, 2) * 4
    assert metadata[::4] == (0, 0, 3, 2, 4, 2) * 4


def test_x25519_python_model_matches_rfc_7748() -> None:
    expected = (
        "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552",
        "95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957",
    )
    for pair, output in zip(reference_x25519.RFC7748_PAIRS, expected, strict=True):
        assert reference_x25519.x25519(*pair).hex() == output

    left = bytes(range(32))
    right = bytes(reversed(range(32)))
    basepoint = bytes([9]) + bytes(31)
    assert reference_x25519.x25519(left, reference_x25519.x25519(right, basepoint)) == (
        reference_x25519.x25519(right, reference_x25519.x25519(left, basepoint))
    )


def test_profile_generators_reject_unknown_profiles() -> None:
    with pytest.raises(ValueError, match="profile must be one of"):
        reference.input_for("sha256", {"length": 1, "profile": "unknown", "seed": 0})

    with pytest.raises(ValueError, match="profile must be one of"):
        reference.input_for(
            "littlefs",
            {
                "operations": 1,
                "profile": "unknown",
                "seed": 0,
            },
        )


@pytest.mark.parametrize("artifacts", [build.ARTIFACTS, build.LONG_ARTIFACTS])
def test_public_manifest_contains_only_public_fields(artifacts: Path) -> None:
    document = json.loads((artifacts / "manifest.json").read_text(encoding="utf-8"))

    assert set(document) == {
        "application_workloads",
        "builder",
        "cases",
        "project_inputs",
        "schema_version",
    }
    assert all(
        set(case)
        == {
            "elf",
            "elf_sha256",
            "expected_output_hex",
            "id",
            "input",
            "input_sha256",
            "instruction_limit",
            "workload",
        }
        for case in document["cases"]
    )


def test_builder_metadata_comes_from_dockerfile(tmp_path: Path) -> None:
    base_image = "rust:1.96.1-slim-bookworm@sha256:" + "a" * 64
    dockerfile = tmp_path / "Dockerfile"
    dockerfile.write_text(f"FROM {base_image}\n", encoding="utf-8")

    metadata = build._builder_metadata(dockerfile)
    assert metadata["base_image"] == base_image
    assert metadata["clang"] == "Debian clang version 14.0.6"
    assert metadata["llvm_ar"] == "Debian LLVM version 14.0.6"
    assert "lld" not in metadata
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
            "--all-features",
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
        assert options["env"]["RVB_C_CLANG"] == build.C_CLANG
        assert options["env"]["RVB_C_LLVM_AR"] == build.C_LLVM_AR


def test_c_workloads_are_feature_gated() -> None:
    manifest = (build.GUEST / "workloads/Cargo.toml").read_text(encoding="utf-8")

    assert "c-workloads = []" in manifest
    for workload in (
        "littlefs",
        "x25519",
        "aes",
        "mont64",
        "picojpeg",
        "sglib",
        "slre",
        "statemate",
        "ud",
    ):
        target = f'name = "{workload}"'
        start = manifest.index(target)
        end = manifest.find("[[bin]]", start + len(target))
        section = manifest[start : None if end == -1 else end]
        assert 'required-features = ["c-workloads"]' in section


def test_canonical_compiles_enable_c_workloads(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    calls = []

    def fake_cargo(arguments: list[str], target_dir: Path, action: str) -> None:
        calls.append((arguments, target_dir, action))

    monkeypatch.setattr(build, "_cargo", fake_cargo)

    build._compile(tmp_path / "short")
    build._compile(tmp_path / "long", long=True)

    assert calls == [
        (
            [
                "build",
                "--frozen",
                "--release",
                "--bins",
                "--features",
                "c-workloads",
            ],
            tmp_path / "short",
            "build",
        ),
        (
            [
                "build",
                "--frozen",
                "--release",
                "--bins",
                "--features",
                "c-workloads,long",
            ],
            tmp_path / "long",
            "long build",
        ),
    ]


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


def test_every_reference_module_is_a_project_input() -> None:
    root = build.ROOT / "reference"
    expected = set(root.glob("*.py"))
    workload_modules = {path.stem for path in expected} - {"__init__", "common"}
    assert workload_modules == set(build.WORKLOADS)
    assert expected <= set(build._project_input_paths())


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

    monkeypatch.setattr(reference_heatshrink, "telemetry", fail_if_called)
    records = (
        reference_heatshrink.MAX_PAYLOAD // reference_heatshrink.TELEMETRY_RECORD_SIZE
        + 1
    )

    with pytest.raises(ValueError, match="16 KiB"):
        reference.input_for("heatshrink", {"records": records, "seed": 0})


def test_reference_models_enforce_authored_input_limits() -> None:
    with pytest.raises(ValueError, match="666 bytes"):
        reference_qrcode.input_for(
            {"length": reference_qrcode.MAX_PAYLOAD + 1, "seed": 0}
        )
    with pytest.raises(ValueError, match="exceeds its limit"):
        reference_qrcode.output_for(bytes(reference_qrcode.MAX_PAYLOAD + 1))

    with pytest.raises(ValueError, match="1..1024"):
        reference_streaming.input_for({"passes": 1, "count": 0, "seed": 0})
    oversized_stream = struct.pack(
        f"<{reference_streaming.MAX_VALUES + 2}I",
        1,
        *range(reference_streaming.MAX_VALUES + 1),
    )
    with pytest.raises(ValueError, match="too many words"):
        reference_streaming.output_for(oversized_stream)

    with pytest.raises(ValueError, match="512 KiB"):
        reference_sha256.input_for(
            {
                "length": reference_sha256.MAX_PAYLOAD + 1,
                "profile": "pseudorandom",
                "seed": 0,
            }
        )
    with pytest.raises(ValueError, match="512 KiB"):
        reference_sha256.output_for(bytes(reference_sha256.MAX_PAYLOAD + 1))


def test_third_party_inputs_exclude_standalone_cargo_state(tmp_path: Path) -> None:
    crate = tmp_path / "dependency"
    source = crate / "src/lib.rs"
    c_source = crate / "library.c"
    header = crate / "library.h"
    provenance = crate / "UPSTREAM.md"
    generated = crate / "target/debug/build/generated.rs"
    for path in (
        crate / "Cargo.toml",
        source,
        c_source,
        header,
        provenance,
        crate / "Cargo.lock",
        generated,
    ):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("")

    assert build._third_party_input_paths(tmp_path) == (
        crate / "Cargo.toml",
        provenance,
        c_source,
        header,
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
