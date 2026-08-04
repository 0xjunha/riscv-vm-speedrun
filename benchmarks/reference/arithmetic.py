"""Reference model for the integer arithmetic diagnostic workload."""

from __future__ import annotations

import struct
from collections.abc import Mapping

from .common import parameters, record, rotl, rotr, u32, words


def input_for(values: Mapping[str, object]) -> bytes:
    return struct.pack("<3I", *parameters(values, ("iterations", "x", "y")))


def output_for(data: bytes) -> bytes:
    values = words(data)
    if len(values) != 3:
        raise ValueError("arithmetic input must contain three words")
    iterations = min(values[0] or 24_000, 120_000)
    x = values[1] ^ 0x243F_6A88
    y = values[2] ^ 0x85A3_08D3
    step = 0x9E37_79B9
    for _ in range(iterations):
        x = u32(x + step)
        x ^= rotl(x, 7)
        y = u32(y + (x ^ (x >> 3)))
        y = rotr(y, 11) ^ x
        x = u32(rotl(x, 5) + y)
        step = u32(step + 0x6D2B_79F5)
    return record(x, y ^ step)
