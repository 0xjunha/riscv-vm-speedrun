"""Reference model for the sequential-memory diagnostic workload."""

from __future__ import annotations

import struct
from collections.abc import Mapping

from .common import lcg, parameters, result, rotl, u32, words

MAX_VALUES = 1024


def input_for(values: Mapping[str, object]) -> bytes:
    passes, count, seed = parameters(values, ("passes", "count", "seed"))
    if not 1 <= count <= MAX_VALUES:
        raise ValueError(f"streaming count must be in 1..{MAX_VALUES}")
    generated = []
    state = seed
    for _ in range(count):
        state = lcg(state)
        generated.append(state)
    return struct.pack(f"<{len(generated) + 1}I", passes, *generated)


def output_for(data: bytes) -> bytes:
    values = words(data)
    if len(values) < 2:
        raise ValueError("streaming input must contain passes and data")
    passes = min(values[0] or 8, 32)
    count = len(values) - 1
    if count > MAX_VALUES:
        raise ValueError("streaming input contains too many words")
    total = 0
    xor = 0
    weighted = 0
    for pass_index in range(passes):
        stride = pass_index + 1
        for index, value in enumerate(values[1:]):
            total = u32(total + value)
            xor ^= rotl(value, index + pass_index)
            weighted = u32(weighted + (value ^ stride))
            stride = u32(stride + 0x9E37_79B9)
    return result(total ^ xor, weighted)
