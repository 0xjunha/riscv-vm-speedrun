"""Reference model for the fixed-overhead diagnostic workload."""

from __future__ import annotations

import struct
from collections.abc import Mapping

from .common import parameters, result, rotl, rotr, words


def input_for(values: Mapping[str, object]) -> bytes:
    return struct.pack("<2I", *parameters(values, ("a", "b")))


def output_for(data: bytes) -> bytes:
    values = words(data)
    if len(values) != 2:
        raise ValueError("tiny input must contain two words")
    value = rotl(values[0], 5) ^ rotr(values[1], 3) ^ 0x7469_6E79
    return result(value, len(data))
