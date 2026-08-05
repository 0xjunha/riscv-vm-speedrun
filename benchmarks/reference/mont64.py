"""Reference model for Embench Montgomery arithmetic."""

from __future__ import annotations

import struct
from collections.abc import Mapping

from .common import MASK32, fold, lcg, profiled_parameters, result

MASK64 = (1 << 64) - 1
RADIX = 1 << 64


def _next_u64(state: int) -> tuple[int, int]:
    state = lcg(state)
    low = state
    state = lcg(state)
    return low | (state << 32), state


def input_for(values: Mapping[str, object]) -> bytes:
    (count, seed), profile = profiled_parameters(
        values, ("records", "seed"), ("mixed", "carry-heavy", "sparse")
    )
    if not 1 <= count <= 512:
        raise ValueError("mont64 records is outside its limit")

    output = bytearray()
    state = seed
    for index in range(count):
        random, state = _next_u64(state)
        if profile == "mixed":
            modulus = (random | (1 << 63) | 1) & MASK64
        elif profile == "carry-heavy":
            modulus = MASK64 - 2 * ((random & 0xFFFF) + 1)
        else:
            modulus = ((random & 0xFFFF_FFFF) | 0x1_0000_0001) & MASK64
        a_raw, state = _next_u64(state)
        b_raw, state = _next_u64(state)
        if profile == "carry-heavy":
            a = modulus - 1 - (a_raw & 0xFF)
            b = modulus - 1 - (b_raw & 0x1FF)
        elif profile == "sparse":
            a = (1 << ((index * 7) % 63)) % modulus
            b = (1 << ((index * 13 + 3) % 63)) % modulus
        else:
            a = a_raw % modulus
            b = b_raw % modulus
        inverse = (-pow(modulus, -1, RADIX)) & MASK64
        output.extend(struct.pack("<QQQQ", a, b, modulus, inverse))
    return bytes(output)


def output_for(data: bytes) -> bytes:
    if not data or len(data) % 32 or len(data) // 32 > 512:
        raise ValueError("mont64 input has an invalid size")
    products = 0x4D4F_4E54
    remainders = 0x3634_5256
    for index, (a, b, modulus, inverse) in enumerate(struct.iter_unpack("<QQQQ", data)):
        if (
            modulus < 3
            or modulus % 2 == 0
            or a >= modulus
            or b >= modulus
            or (modulus * inverse) & MASK64 != MASK64
        ):
            raise ValueError("mont64 input contains an invalid record")
        product = a * b * pow(RADIX, -1, modulus) % modulus
        remainder = ((a << 64) | b) % modulus
        products = fold(products, (product ^ (product >> 32)) & MASK32, index)
        remainders = fold(remainders, (remainder ^ (remainder >> 32)) & MASK32, index)
    return result(products, remainders)
