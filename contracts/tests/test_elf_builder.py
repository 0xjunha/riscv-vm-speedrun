from __future__ import annotations

import json
from pathlib import Path

import build as contract_build
import pytest
from elf_builder import (
    ELF_HEADER,
    PF_R,
    PROGRAM_HEADER,
    SECTION_HEADER,
    build_elf,
    validate_elf,
)

CONTRACTS = Path(__file__).parent.parent


def test_every_authored_elf_variant_has_one_expected_result() -> None:
    cases = json.loads((CONTRACTS / "cases.json").read_text())["cases"]

    for case in cases:
        elf = build_elf(b"\x73\x00\x00\x00", case.get("elf_variant", "default"))
        assert validate_elf(elf) == case.get("expected_rejection"), case["id"]


def test_default_elf_is_deterministic() -> None:
    code = b"\x73\x00\x00\x00"

    assert build_elf(code) == build_elf(code)
    assert validate_elf(build_elf(code)) is None


def test_unknown_variant_is_rejected() -> None:
    try:
        build_elf(b"\x73\x00\x00\x00", "unknown")
    except ValueError as error:
        assert str(error) == "unknown ELF variant: unknown"
    else:
        raise AssertionError("unknown ELF variant was accepted")


def test_structural_rejection_variants_isolate_the_intended_rule() -> None:
    code = b"\x73\x00\x00\x00"
    overlap = build_elf(code, "overlapping-segments")
    header = ELF_HEADER.unpack_from(overlap)
    phoff = header[5]
    second_segment = PROGRAM_HEADER.unpack_from(
        overlap,
        phoff + PROGRAM_HEADER.size,
    )
    assert second_segment[6] == PF_R

    extended_programs = build_elf(code, "extended-program-headers")
    header = ELF_HEADER.unpack_from(extended_programs)
    section_zero = SECTION_HEADER.unpack_from(extended_programs, header[6])
    assert header[10] == 0xFFFF
    assert section_zero[7] == 2

    extended_sections = build_elf(code, "extended-section-numbering")
    header = ELF_HEADER.unpack_from(extended_sections)
    section_zero = SECTION_HEADER.unpack_from(extended_sections, header[6])
    assert header[12] == 0
    assert section_zero[5] == 1


def test_authored_case_validation_rejects_malformed_run(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    cases = tmp_path / "cases.json"
    cases.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "cases": [
                    {
                        "id": "bad",
                        "spec": ["rv32im-eei.md §9"],
                        "kind": "execute",
                        "source": "guest/exit.S",
                        "runs": [{"instruction_limit": -1, "result": {}}],
                    }
                ],
            }
        )
    )
    monkeypatch.setattr(contract_build, "CASES", cases)

    with pytest.raises(RuntimeError, match="run"):
        contract_build._load_cases()
