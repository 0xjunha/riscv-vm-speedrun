from __future__ import annotations

import hashlib
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
    build.check_all()


def test_reference_records_are_stable() -> None:
    expected_outputs = {
        "arithmetic": "5256423103000000b45a03028dac6b0e",
        "depthconv": "525642310b0000003e48b268c2e8ec02",
        "depthconv-saturated": "525642310b0000008ef0d57fb07d2188",
        "depthconv-sparse": "525642310b000000638fbeb3fd2ec866",
        "dijkstra": "525642310d0000002ec00100de5dd2b4",
        "dijkstra-clustered": "525642310d00000048c502004791a6bd",
        "dijkstra-hub": "525642310d000000f06c0300ce126d71",
        "heatshrink": "5256423109000000a599a52541de76c0",
        "heatshrink-bursty": "52564231090000000ed87675caae4b5d",
        "heatshrink-steady": "5256423109000000679d0d473d51d64a",
        "littlefs": "5256423117000000b66b16f6b49714df",
        "littlefs-append": "52564231170000005f3240a9efdec283",
        "littlefs-metadata": "52564231170000008d2724489dcb2621",
        "qrcode": "5256423111000000a80400007d9fde3e",
        "qrcode-capacity": "525642311100000094120000a1fca83a",
        "qrcode-compact": "5256423111000000bc0100005a25ccd4",
        "sha256": "52564231070000002b7fa3a8f050c784",
        "sha256-pages": "52564231070000001d2e78d57e301280",
        "sha256-sparse": "52564231070000000ce81159f6c0cedc",
        "sort_records": "525642310f0000001f22b550eb0131eb",
        "sort_records-ascending": "525642310f0000008432ded320cac130",
        "sort_records-duplicates": "525642310f000000d5e64af35327ca28",
        "streaming": "52564231050000001eb9132b00d8beff",
        "tiny": "525642310100000024bb34a108000000",
        "x25519": "5256423119000000825a70dd62467e39",
        "x25519-carry": "5256423119000000d65c892bb645178b",
        "x25519-generated": "525642311900000055d6565d37fa59b9",
    }
    expected_inputs = {
        "arithmetic": "a34582326b5b9036c0560479357ddfaf3314d354cf320433cc4fd582d6704ee0",
        "depthconv": "155ebfc398f1f13b0bff85e742349ea8ac12681dc24386f59563a2ec98d60988",
        "depthconv-saturated": "a2c3eb00b3ad57ff1ddaf534331c62ca09df0da67811a6e1e4d1a2269f499bf0",
        "depthconv-sparse": "9aad6c9ce0a7194d11ae48a2c022adfd7180c1824147f865894cd1682f494c43",
        "dijkstra": "8fa982e9cbc9f5b3f36f394aafb5145f16deabeba7ba706a903eb770b8417d11",
        "dijkstra-clustered": "a89d6592e353aefb3ca52fe1aa59797aeb2461fdce417d74920a9bcb919e67b9",
        "dijkstra-hub": "f879d70f1e08ad9f553cedb3b413cfc51cbc35936b4e091c58d028b0542d11d3",
        "heatshrink": "d1debdd4a46789fda12debff4884790d8f03a7007b3ccc1bee5adb6d456e30f0",
        "heatshrink-bursty": "ca882191b992b7aa50d7d1f590a3f01c1d730240b9ad5c64bc5ef60300ed99ab",
        "heatshrink-steady": "15e92e12695fafc0fa927ff26b7640f4128e43418fb904972c36b555582cee2c",
        "littlefs": "5d2ceef91ad18cf728d7e361218a6c9a01379d5156841e2a8b9c729a28a25131",
        "littlefs-append": "e6758de25d7f17d0c256fd2fbf82a32e0877fa009a2278b319a76843a987c0ca",
        "littlefs-metadata": "866df6e81a6deb21528fcc47bbb14351c1da5fba34122f3e68152fe363329229",
        "qrcode": "95dea1e7f058ed0a69642b2182868a8b327ece4233e8101ed1e6d2bf4dec67e2",
        "qrcode-capacity": "0a5da553401adc7aac0e2a218fb0b12654f3e317c9b56005d0270278994f71ef",
        "qrcode-compact": "89e8eab7b14478f2cf25ee1fb6ca8a451d304e49e575e7f745d952205dbefcb4",
        "sha256": "a0fa6f78e072a5732ee155fcf8c09515b2cbcd2f01e12d2a3f475f3e49cb81ce",
        "sha256-pages": "1ad4f782ce8c8e0534d616798a48ccdf9581df07bb914476d84206d2b26da9dc",
        "sha256-sparse": "cc0490d086833ff99862d2ac51d458455c84f9a1b574a3db66db7db0dcd06428",
        "sort_records": "7e89d8a2e6d8e5ba34dc7bbc17ccf589f411ffab93a1c706cdb622adfd8d6b84",
        "sort_records-ascending": "5dd2ac84af3321a5c5b310f8f6ee962e56b07161853d6a81b62048476c3388bb",
        "sort_records-duplicates": "8aa422f1ec989651f9292c090f47f0b0f4957b05f274bba587c06dfa85ed04c3",
        "streaming": "74d6ae3773394cd22a118b599c926dae2b4d519ce9e4be5d69768fc04c350616",
        "tiny": "fdd232547ceee35add35783ca7d518d2a49abccd0b60b028c8a9c629eba6201f",
        "x25519": "d946e1feb68ddbcedcbf0da908f882b138a85b61f283bdc858b5e14adab917d6",
        "x25519-carry": "97faa38bc6e8279f984a2ecdc8e85b231adcf5010be9454987a2da88337b4ef4",
        "x25519-generated": "6757d5c857ae48b117d40608540de3c25aa1877af13cb17b538a689be1895c5a",
    }
    cases = build.load_cases()
    assert {case.id for case in cases} == set(expected_outputs) == set(expected_inputs)
    for case in cases:
        data = reference.input_for(case.workload, case.parameters)
        assert data == reference.input_for(case.workload, case.parameters)
        assert hashlib.sha256(data).hexdigest() == expected_inputs[case.id]
        assert (
            reference.output_for(case.workload, data).hex() == expected_outputs[case.id]
        )


def test_application_case_profiles_are_diversified() -> None:
    cases = build.load_cases()
    counts = {
        workload: sum(case.workload == workload for case in cases)
        for workload in (
            "sha256",
            "heatshrink",
            "depthconv",
            "dijkstra",
            "sort_records",
            "littlefs",
            "x25519",
            "qrcode",
        )
    }
    assert counts == dict.fromkeys(counts, 3)


def test_workload_categories_are_complete_and_consistent() -> None:
    cases = build.load_cases()
    categories = {
        workload: {case.category for case in cases if case.workload == workload}
        for workload in build.WORKLOADS
    }
    assert all(len(category) == 1 for category in categories.values())
    assert {
        workload
        for workload, category in categories.items()
        if category == {"diagnostic"}
    } == {"tiny", "arithmetic", "streaming"}
    assert all(
        category == {"application"}
        for workload, category in categories.items()
        if workload not in {"tiny", "arithmetic", "streaming"}
    )


def test_authored_profiles_have_their_intended_shapes() -> None:
    repeated = reference._firmware_payload(768, 1, "repeated-pages")
    assert repeated[:256] == repeated[256:512] == repeated[512:]

    sparse_flash = reference._firmware_payload(1024, 2, "sparse-flash")
    assert sparse_flash.count(0xFF) >= 950

    ascending = struct.unpack("<16I", reference._records(8, 3, "ascending"))
    assert ascending[::2] == tuple(range(8))
    duplicates = struct.unpack("<128I", reference._records(64, 4, "duplicate-heavy"))
    assert len(set(duplicates[::2])) <= 16

    hub = struct.unpack("<64H", reference._graph(8, 0, 5, "hub"))
    assert all(hub[target] for target in range(1, 8))
    assert all(hub[source * 8] for source in range(1, 8))

    mixed = struct.unpack("<80I", reference._littlefs_operations(20, 9, "mixed"))
    append = struct.unpack(
        "<64I", reference._littlefs_operations(16, 10, "append-heavy")
    )
    metadata = struct.unpack(
        "<96I", reference._littlefs_operations(24, 11, "metadata-churn")
    )
    assert mixed[::4] == (0, 1, 2, 3, 4) * 4
    assert append[::4] == (0, 1, 1, 2) * 4
    assert metadata[::4] == (0, 0, 3, 2, 4, 2) * 4


def test_x25519_python_model_matches_rfc_7748() -> None:
    expected = (
        "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552",
        "95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957",
    )
    for pair, output in zip(reference.X25519_RFC7748_PAIRS, expected, strict=True):
        assert reference._x25519(*pair).hex() == output

    left = bytes(range(32))
    right = bytes(reversed(range(32)))
    basepoint = bytes([9]) + bytes(31)
    assert reference._x25519(left, reference._x25519(right, basepoint)) == (
        reference._x25519(right, reference._x25519(left, basepoint))
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
                "repetitions": 1,
                "seed": 0,
            },
        )


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
    for workload in ("littlefs", "x25519"):
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
