"""Reference model for the SHA-256 firmware workload."""

from __future__ import annotations

import hashlib
import struct
from collections.abc import Mapping

from .common import generated_bytes, header, lcg, profiled_parameters, record


def firmware_payload(length: int, seed: int, profile: str) -> bytes:
    if profile == "pseudorandom":
        return generated_bytes(length, seed)
    if profile == "repeated-pages":
        if length == 0:
            return b""
        page = generated_bytes(min(length, 256), seed)
        return (page * ((length + len(page) - 1) // len(page)))[:length]
    if profile == "sparse-flash":
        output = bytearray([0xFF]) * length
        state = seed
        for offset in range(0, length, 256):
            for index in range(offset, min(offset + 16, length)):
                state = lcg(state)
                output[index] = state >> 24
        return bytes(output)
    raise ValueError(f"unknown firmware profile: {profile}")


def input_for(values: Mapping[str, object]) -> bytes:
    (length, seed), profile = profiled_parameters(
        values,
        ("length", "seed"),
        ("pseudorandom", "repeated-pages", "sparse-flash"),
    )
    if length > 512 * 1024:
        raise ValueError("sha256 length must not exceed 512 KiB")
    return struct.pack("<I", length) + firmware_payload(length, seed, profile)


def output_for(data: bytes) -> bytes:
    length = header(data, "sha256")
    payload = data[4:]
    if len(payload) != length:
        raise ValueError("sha256 input length is invalid")
    digest = hashlib.sha256(payload).digest()
    return record(
        int.from_bytes(digest[:4], "big"),
        int.from_bytes(digest[-4:], "big"),
    )
