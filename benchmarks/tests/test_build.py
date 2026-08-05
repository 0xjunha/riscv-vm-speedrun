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


def test_reference_records_are_stable() -> None:
    expected_outputs = {
        "aes": "0c975cae0905be88",
        "aes-counter": "72313a5ce8be2c89",
        "aes-zero-heavy": "b63eee91ee7320c5",
        "arithmetic": "b45a03028dac6b0e",
        "depthconv": "3e48b268c2e8ec02",
        "depthconv-saturated": "8ef0d57fb07d2188",
        "depthconv-sparse": "638fbeb3fd2ec866",
        "dijkstra": "2ec00100de5dd2b4",
        "dijkstra-clustered": "48c502004791a6bd",
        "dijkstra-hub": "f06c0300ce126d71",
        "heatshrink": "a599a52541de76c0",
        "heatshrink-bursty": "0ed87675caae4b5d",
        "heatshrink-steady": "679d0d473d51d64a",
        "littlefs": "4a8dcf44fe1adb9b",
        "littlefs-append": "e0f2f3e40f2c3167",
        "littlefs-metadata": "9dcb262100000000",
        "mont64": "0179c7a6cc0b8bc6",
        "mont64-carry": "d62305621894aa4a",
        "mont64-sparse": "8ca65f23ed3d6a7c",
        "picojpeg-420": "d811167584c1e632",
        "picojpeg-444": "8ce033fd5cf73611",
        "picojpeg-grayscale": "5815a6df270bafec",
        "qrcode": "a80400007d9fde3e",
        "qrcode-capacity": "94120000a1fca83a",
        "qrcode-compact": "bc0100005a25ccd4",
        "sha256": "2b7fa3a8f050c784",
        "sha256-pages": "1d2e78d57e301280",
        "sha256-sparse": "0ce81159f6c0cedc",
        "sglib": "7aadf261cad2afb1",
        "sglib-clustered": "bb93921c56022cf2",
        "sglib-ordered": "38b0cdf11670aed5",
        "slre": "091b485aa32fed10",
        "slre-branches": "fa6ea19dbaba1a17",
        "slre-classes": "1288ea523f270173",
        "sort_records": "1f22b550eb0131eb",
        "sort_records-ascending": "8432ded320cac130",
        "sort_records-duplicates": "d5e64af35327ca28",
        "streaming": "1eb9132b00d8beff",
        "statemate": "c6f43c67582ea738",
        "statemate-mixed": "c6f43c678c66ef53",
        "statemate-obstruction": "c6f43c67167040d6",
        "tiny": "24bb34a108000000",
        "ud": "f2f3624e3db4943d",
        "ud-banded": "395a2559ed6f7ea0",
        "ud-positive": "5561791df2293fce",
        "x25519": "62467e39743a7d91",
        "x25519-carry": "b645178bed0bf368",
        "x25519-generated": "37fa59b9e5835650",
    }
    expected_inputs = {
        "aes": "3bab5e2691f8f5bc19afbe84c02c1ea23dea22ffb99a71f6d1f0539d983f3306",
        "aes-counter": "9c782eef84d4a91e20621f5c372a9e7d28ac81c530d29839767083e8624727d1",
        "aes-zero-heavy": "8be647910c538aa5a12af62d7ebb5af09b12f14f18944b42481b1ad6f9ec775e",
        "arithmetic": "a34582326b5b9036c0560479357ddfaf3314d354cf320433cc4fd582d6704ee0",
        "depthconv": "155ebfc398f1f13b0bff85e742349ea8ac12681dc24386f59563a2ec98d60988",
        "depthconv-saturated": "a2c3eb00b3ad57ff1ddaf534331c62ca09df0da67811a6e1e4d1a2269f499bf0",
        "depthconv-sparse": "9aad6c9ce0a7194d11ae48a2c022adfd7180c1824147f865894cd1682f494c43",
        "dijkstra": "8fa982e9cbc9f5b3f36f394aafb5145f16deabeba7ba706a903eb770b8417d11",
        "dijkstra-clustered": "a89d6592e353aefb3ca52fe1aa59797aeb2461fdce417d74920a9bcb919e67b9",
        "dijkstra-hub": "f879d70f1e08ad9f553cedb3b413cfc51cbc35936b4e091c58d028b0542d11d3",
        "heatshrink": "8cbd9b27bb9d355ce23ef6bf57daff43d9934a07d1b883859bf7dc449d91bf09",
        "heatshrink-bursty": "6f5df1eb37990a8c49b56e573afb726ebf4a6459bfa114e8128a62929963e660",
        "heatshrink-steady": "170402a094970735e1a39336bd358ddfb233e5541ed2e73ebd0bfe9f88726811",
        "littlefs": "f368194ee37796e7d04b0d9bb53317c56ed418f71f4232490cedd7d9974b4662",
        "littlefs-append": "5e9964658d0e2e7f08c9ffa723de27d613e50102f14e17c268a5df42142afbf2",
        "littlefs-metadata": "ded3e217a6775c071726378deeb1467e1a1f35a5beaa0831771b64ed38496278",
        "mont64": "f6cc0c89ae15e4b4bad989d50a7a093dbe2f760aaf397aa9ebda1ee803be1be3",
        "mont64-carry": "696ed20b377c4a76446e82ea9d00f347b94a9ffcb49d1037bd1a9fe94fd4dfab",
        "mont64-sparse": "eaf95fda8cfe492e7dead1db3fbc1520feaaa8e36901a6b4228f951264629d68",
        "picojpeg-420": "9ce2cdfed2bbbb57f2ad72e39a8f8503bdef0099ec660b1321f85dc14638600d",
        "picojpeg-444": "2156cd419882aef35459c99feb3d76da9ba0b2c3b58076183aaf07a86f768e8e",
        "picojpeg-grayscale": "658a1bc1ba62f066d4c79dbaa791ab166edf6fc81b3b5367f5220531534f3f29",
        "qrcode": "f0a14d349af37ba22c7c2485ceb7d5462deda8d66f4cb041ddb57b3049c82151",
        "qrcode-capacity": "84570f120cce49bca08f3b93bf7ebdc7114e1819813f143d86dd1b30dbab9b42",
        "qrcode-compact": "fb7fe91cb99d4dd5ae7eaa1ca33ec3b5e32ed24fc221c1e67f803e57ec0db309",
        "sha256": "a8a37f2bd7db770d691baa29f084690d7ebc24cd8aede98796a1dbb184c750f0",
        "sha256-pages": "d5782e1d4d8b3aaccb9897f0ac072e50c3a3f80ced5134e4d5aafb7c8012307e",
        "sha256-sparse": "5911e80c2a5efdf6c01d920b1ead7079f901aabbcb0c39ed121e836ddccec0f6",
        "sglib": "974769b1696724890bded277ef81d7c00981713a8a1a2f96b7808751c0b086ce",
        "sglib-clustered": "edc5380bcd19a7eb69fd447d762506356cceea5dee3dff9b01512e1bb3cf42c1",
        "sglib-ordered": "082e8523cedb83caa0cfdcff3555a954c98225be56d5546a9305f53b0dce16cf",
        "slre": "ac861289540285bc48cea0cbd0481f70411a743372adab299bce6eeb92e7b723",
        "slre-branches": "ccc367e00eb69146af3a3b47727134c768ec0a71e82d99527ad785e67a0fa1a9",
        "slre-classes": "d807bfe0e6795bf0118e2b6844080b32457cc04192d485df3b3a2fbbeefa2dcc",
        "sort_records": "9d548fdfd0f5671b276e69a22dc26cba081380007ea6ad2a1d518074d9ad53c8",
        "sort_records-ascending": "3b4740b09dd546ec08de4142cb3f9b20d722382a7af636ddc64976a241b8c59b",
        "sort_records-duplicates": "f79217deeed5ec37b0d2f1d6c5276e38fdefdee99059ca53c53a8dfdc8f7998a",
        "streaming": "d87ee0867bab9a16d0c06edf2fc9e02f6fa54057b3e405bd0a3dc0f6baa6023c",
        "statemate": "0cdede353c8f61f91b21492a528236428436b93ebe1c214244c71bb739fb4f80",
        "statemate-mixed": "2928b4476fad7f765ae005f89f765ac9f799f63860ed204efbc3cb2a9517bf9c",
        "statemate-obstruction": "d0ebabeee0b1293a27974b080cfe8ed279822ada282df6d51ca340d761ae1fe8",
        "tiny": "fdd232547ceee35add35783ca7d518d2a49abccd0b60b028c8a9c629eba6201f",
        "ud": "736e84cb4a5481cc2166c752da4fa2b21410e285c8de63f302b1237fc8acd02e",
        "ud-banded": "67b9a26f7a67637b73c94b728a008ebd90e3ef9053666bbc2710bb3981da915d",
        "ud-positive": "278981e543067c53258bd1f52973d95a4cdcac1dc3795fec0ac832a93633e84b",
        "x25519": "fcc43432e6e38d6dba6d416ea9fcbb78647afcd79bd3c7e0357a9853357fdd9c",
        "x25519-carry": "d8f8d06ea49097bacd9d941a8e584f269aa15b6d2ec2d8b644c6a86346115e56",
        "x25519-generated": "b7ea57b2246d9740e41fa337222dd67ca99f444c6d6de753271618f0cf0f9ebf",
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


def test_aes256_reference_known_answer() -> None:
    key = bytes.fromhex(
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
    )
    plaintext = bytes.fromhex("00112233445566778899aabbccddeeff")
    assert reference_aes._encrypt_block(key, plaintext).hex() == (
        "8ea2b7ca516745bfeafc49904b496089"
    )


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
