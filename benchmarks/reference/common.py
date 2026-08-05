"""Shared primitives for deterministic benchmark reference models."""

from __future__ import annotations

import struct
from collections.abc import Mapping

MASK32 = 0xFFFF_FFFF


def u32(value: int) -> int:
    return value & MASK32


def rotl(value: int, shift: int) -> int:
    shift &= 31
    return u32((value << shift) | (value >> ((32 - shift) & 31)))


def rotr(value: int, shift: int) -> int:
    shift &= 31
    return u32((value >> shift) | (value << ((32 - shift) & 31)))


def parameter(parameters: Mapping[str, object], name: str) -> int:
    value = parameters.get(name)
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not 0 <= value <= MASK32
    ):
        raise ValueError(f"{name} must be an unsigned 32-bit integer")
    return value


def parameters(values: Mapping[str, object], names: tuple[str, ...]) -> tuple[int, ...]:
    if set(values) != set(names):
        raise ValueError(f"parameters must be {', '.join(names)}")
    return tuple(parameter(values, name) for name in names)


def profiled_parameters(
    values: Mapping[str, object],
    names: tuple[str, ...],
    profiles: tuple[str, ...],
) -> tuple[tuple[int, ...], str]:
    expected = set(names)
    if set(values) not in (expected, expected | {"profile"}):
        raise ValueError(
            f"parameters must be {', '.join(names)}, with optional profile"
        )
    profile = values.get("profile", profiles[0])
    if not isinstance(profile, str) or profile not in profiles:
        raise ValueError(f"profile must be one of {', '.join(profiles)}")
    return tuple(parameter(values, name) for name in names), profile


def lcg(state: int) -> int:
    return u32(state * 1_664_525 + 1_013_904_223)


def fold(accumulator: int, value: int, index: int) -> int:
    return u32((rotl(accumulator, 5) ^ value) + 0x9E37_79B9 + index)


def generated_bytes(length: int, seed: int) -> bytes:
    output = bytearray(length)
    state = seed
    for index in range(length):
        state = lcg(state)
        output[index] = state >> 24
    return bytes(output)


def words(data: bytes) -> tuple[int, ...]:
    if len(data) % 4:
        raise ValueError("workload input length must be a multiple of four")
    return tuple(value[0] for value in struct.iter_unpack("<I", data))


def header(data: bytes, workload: str) -> int:
    if len(data) < 4:
        raise ValueError(f"{workload} input is missing its header")
    return struct.unpack_from("<I", data)[0]


def result(low: int, high: int = 0) -> bytes:
    """Encode one little-endian 64-bit result from two 32-bit observations."""

    return struct.pack("<Q", u32(low) | (u32(high) << 32))
