"""Inputs and pinned outputs for the upstream Embench SLRE engine."""

from __future__ import annotations

import hashlib
import struct
from collections.abc import Mapping

from .common import profiled_parameters

_EXPECTED = {
    "ac861289540285bc48cea0cbd0481f70411a743372adab299bce6eeb92e7b723": bytes.fromhex(
        "091b485aa32fed10"
    ),
    "d807bfe0e6795bf0118e2b6844080b32457cc04192d485df3b3a2fbbeefa2dcc": bytes.fromhex(
        "1288ea523f270173"
    ),
    "ccc367e00eb69146af3a3b47727134c768ec0a71e82d99527ad785e67a0fa1a9": bytes.fromhex(
        "fa6ea19dbaba1a17"
    ),
}


def _record(pattern: str, text: str) -> bytes:
    encoded_pattern = pattern.encode("ascii")
    encoded_text = text.encode("ascii")
    return (
        bytes([len(encoded_pattern)])
        + struct.pack("<H", len(encoded_text))
        + encoded_pattern
        + encoded_text
    )


def input_for(values: Mapping[str, object]) -> bytes:
    (rounds, seed), profile = profiled_parameters(
        values, ("rounds", "seed"), ("embench", "classes", "branches")
    )
    if not 1 <= rounds <= 32:
        raise ValueError("slre rounds is outside its limit")
    suites = {
        "embench": (
            ("(ab)+", "abbbababaabccababcacbcbcbabbabcbabcabcbbcbbac"),
            ("(b.+)+", "bbabacabbabccababcabcbabbac"),
            ("a[ab]*", "ccaaababbac"),
            ("([ab^c][ab^c])+", "abacbcbabbabcbabcabcbb"),
        ),
        "classes": (
            ("[a-f]+[0-9]+", "prefix-acdeff2048-suffix"),
            ("\\d+\\s+([a-z]+)", "id=7319 telemetry"),
            ("(?i)(sensor)+", "xxSeNsOrsensorYY"),
            ("[^x]+x", "branch-heavy-text-x"),
        ),
        "branches": (
            ("(alpha|beta|gamma)+", "zzalphabetagammaalpha"),
            ("(ab|ac|ad)+z", "xxabacabadadz"),
            ("([0-3]+|[a-d]+)+", "qq0123abcd2301"),
            ("(a.*b|b.*a)", "prefix-a-branch-b-tail"),
        ),
    }
    records = []
    for repeat in range(rounds):
        for pattern, text in suites[profile]:
            suffix = chr(ord("a") + ((seed + repeat) % 5))
            records.append(_record(pattern, text + suffix))
    return b"".join(records)


def output_for(data: bytes) -> bytes:
    try:
        return _EXPECTED[hashlib.sha256(data).hexdigest()]
    except KeyError:
        raise ValueError("slre input is not an authored case") from None
