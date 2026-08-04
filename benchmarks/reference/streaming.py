"""Reference model for the sequential-memory diagnostic workload."""

from __future__ import annotations

import struct
from collections.abc import Mapping

from .common import lcg, parameters, record, rotl, u32, words


def input_for(values: Mapping[str, object]) -> bytes:
    passes, count, seed = parameters(values, ("passes", "count", "seed"))
    if count > 1024:
        raise ValueError("streaming count must not exceed 1024")
    generated = []
    state = seed
    for _ in range(count):
        state = lcg(state)
        generated.append(state)
    return struct.pack(f"<{len(generated) + 2}I", passes, count, *generated)


def output_for(data: bytes) -> bytes:
    values = words(data)
    if len(values) < 2:
        raise ValueError("streaming input must contain a header")
    passes = min(values[0] or 8, 32)
    available = len(values) - 2
    count = min(values[1] or min(available, 256), min(available, 1024))
    total = 0
    xor = 0
    weighted = 0
    for pass_index in range(passes):
        stride = pass_index + 1
        for index, value in enumerate(values[2 : 2 + count]):
            total = u32(total + value)
            xor ^= rotl(value, index + pass_index)
            weighted = u32(weighted + (value ^ stride))
            stride = u32(stride + 0x9E37_79B9)
    return record(total ^ xor, weighted)
