"""Reference model for stable bounded-record sorting."""

from __future__ import annotations

import struct
from collections.abc import Mapping

from .common import lcg, profiled_parameters, result, rotl, u32


def records(count: int, seed: int, profile: str = "uniform") -> bytes:
    values = []
    state = seed
    for index in range(count):
        state = lcg(state)
        if profile == "uniform":
            key = (state ^ (state >> 16)) & 0xFFFF
        elif profile == "ascending":
            key = index
        elif profile == "duplicate-heavy":
            key = (state >> 28) & 0xF
        else:
            raise ValueError(f"unknown record profile: {profile}")
        state = lcg(state)
        values.extend((key, state))
    return struct.pack(f"<{len(values)}I", *values)


def input_for(values: Mapping[str, object]) -> bytes:
    (count, passes, seed), profile = profiled_parameters(
        values,
        ("count", "passes", "seed"),
        ("uniform", "ascending", "duplicate-heavy"),
    )
    if not 2 <= count <= 2048 or not 1 <= passes <= 16:
        raise ValueError("sort_records count or passes is outside its limit")
    return struct.pack("<I", passes) + records(count, seed, profile)


def output_for(data: bytes) -> bytes:
    if len(data) < 20 or (len(data) - 4) % 8:
        raise ValueError("sort_records input has an invalid size")
    passes = struct.unpack_from("<I", data)[0]
    count = (len(data) - 4) // 8
    if count > 2048 or not 1 <= passes <= 16:
        raise ValueError("sort_records input is outside its limits")
    words = struct.unpack_from(f"<{count * 2}I", data, 4)
    aggregate = 0
    final_fold = 0
    for pass_index in range(passes):
        mask = u32(pass_index * 0x9E37_79B9)
        sorted_records = sorted(
            ((words[index * 2] ^ mask, words[index * 2 + 1]) for index in range(count)),
            key=lambda item: item[0],
        )
        folded = 0x811C_9DC5
        for key, value in sorted_records:
            folded = rotl(folded, 5) ^ key
            folded = u32(folded * 0x0100_0193) ^ value
        aggregate ^= rotl(folded, pass_index)
        final_fold = folded
    return result(aggregate, final_fold)
