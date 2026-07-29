"""Independent input and expected-output models for the public workloads."""

from __future__ import annotations

import struct
from collections.abc import Mapping

MASK32 = 0xFFFF_FFFF
OUTPUT_MAGIC = 0x3142_5652  # bytes: RVB1


def _u32(value: int) -> int:
    return value & MASK32


def _rotl(value: int, shift: int) -> int:
    return _u32((value << shift) | (value >> (32 - shift)))


def _rotr(value: int, shift: int) -> int:
    return _u32((value >> shift) | (value << (32 - shift)))


def _parameter(parameters: Mapping[str, object], name: str) -> int:
    value = parameters.get(name)
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not 0 <= value <= MASK32
    ):
        raise ValueError(f"{name} must be an unsigned 32-bit integer")
    return value


def input_for(workload: str, parameters: Mapping[str, object]) -> bytes:
    """Encode one authored case as the guest's little-endian input."""

    if workload == "tiny":
        if set(parameters) != {"a", "b"}:
            raise ValueError("tiny parameters must be a and b")
        words = [_parameter(parameters, "a"), _parameter(parameters, "b")]
    elif workload == "arithmetic":
        if set(parameters) != {"iterations", "x", "y"}:
            raise ValueError("arithmetic parameters must be iterations, x, and y")
        words = [
            _parameter(parameters, "iterations"),
            _parameter(parameters, "x"),
            _parameter(parameters, "y"),
        ]
    elif workload == "streaming":
        if set(parameters) != {"passes", "count", "seed"}:
            raise ValueError("streaming parameters must be passes, count, and seed")
        passes = _parameter(parameters, "passes")
        count = _parameter(parameters, "count")
        if count > 1024:
            raise ValueError("streaming count must not exceed 1024")
        state = _parameter(parameters, "seed")
        values = []
        for _ in range(count):
            state = _u32(state * 1_664_525 + 1_013_904_223)
            values.append(state)
        words = [passes, count, *values]
    else:
        raise ValueError(f"unknown workload: {workload}")
    return struct.pack(f"<{len(words)}I", *words)


def _words(data: bytes) -> tuple[int, ...]:
    if len(data) % 4:
        raise ValueError("workload input length must be a multiple of four")
    return tuple(value[0] for value in struct.iter_unpack("<I", data))


def _record(family: int, result: int, auxiliary: int) -> bytes:
    return struct.pack("<IIII", OUTPUT_MAGIC, family, _u32(result), _u32(auxiliary))


def output_for(workload: str, data: bytes) -> bytes:
    """Compute the guest's result without executing guest code."""

    words = _words(data)
    if workload == "tiny":
        if len(words) != 2:
            raise ValueError("tiny input must contain two words")
        result = _rotl(words[0], 5) ^ _rotr(words[1], 3) ^ 0x7469_6E79
        return _record(1, result, len(data))

    if workload == "arithmetic":
        if len(words) != 3:
            raise ValueError("arithmetic input must contain three words")
        iterations = min(words[0] or 24_000, 120_000)
        x = words[1] ^ 0x243F_6A88
        y = words[2] ^ 0x85A3_08D3
        step = 0x9E37_79B9
        for _ in range(iterations):
            x = _u32(x + step)
            x ^= _rotl(x, 7)
            y = _u32(y + (x ^ (x >> 3)))
            y = _rotr(y, 11) ^ x
            x = _u32(_rotl(x, 5) + y)
            step = _u32(step + 0x6D2B_79F5)
        return _record(3, x, y ^ step)

    if workload == "streaming":
        if len(words) < 2:
            raise ValueError("streaming input must contain a header")
        passes = min(words[0] or 8, 32)
        available = len(words) - 2
        count = min(words[1] or min(available, 256), min(available, 1024))
        total = 0
        xor = 0
        weighted = 0
        for pass_index in range(passes):
            stride = pass_index + 1
            for index, value in enumerate(words[2 : 2 + count]):
                total = _u32(total + value)
                xor ^= _rotl(value, (index + pass_index) & 31)
                weighted = _u32(weighted + (value ^ stride))
                stride = _u32(stride + 0x9E37_79B9)
        return _record(5, total ^ xor, weighted)

    raise ValueError(f"unknown workload: {workload}")
