"""Independent RFC 7748 model for the Monocypher X25519 workload."""

from __future__ import annotations

import struct
import zlib
from collections.abc import Mapping

from .common import fold, header, lcg, profiled_parameters, record

RFC7748_PAIRS = (
    (
        bytes.fromhex(
            "a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4"
        ),
        bytes.fromhex(
            "e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c"
        ),
    ),
    (
        bytes.fromhex(
            "4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d"
        ),
        bytes.fromhex(
            "e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493"
        ),
    ),
)


def x25519(scalar: bytes, coordinate: bytes) -> bytes:
    if len(scalar) != 32 or len(coordinate) != 32:
        raise ValueError("X25519 inputs must be 32 bytes")
    prime = 2**255 - 19
    key = int.from_bytes(scalar, "little")
    key &= (1 << 255) - 8
    key |= 1 << 254
    point = int.from_bytes(coordinate, "little") & ((1 << 255) - 1)
    x_2, z_2, x_3, z_3, swap = 1, 0, point, 1, 0
    for bit in range(254, -1, -1):
        current = (key >> bit) & 1
        swap ^= current
        if swap:
            x_2, x_3 = x_3, x_2
            z_2, z_3 = z_3, z_2
        swap = current
        a = (x_2 + z_2) % prime
        aa = a * a % prime
        b = (x_2 - z_2) % prime
        bb = b * b % prime
        difference = (aa - bb) % prime
        c = (x_3 + z_3) % prime
        d = (x_3 - z_3) % prime
        da = d * a % prime
        cb = c * b % prime
        x_3 = (da + cb) ** 2 % prime
        z_3 = point * (da - cb) ** 2 % prime
        x_2 = aa * bb % prime
        z_2 = difference * (aa + 121_665 * difference) % prime
    if swap:
        x_2, x_3 = x_3, x_2
        z_2, z_3 = z_3, z_2
    result = x_2 * pow(z_2, prime - 2, prime) % prime
    return result.to_bytes(32, "little")


def key_material(state: int) -> tuple[bytes, int]:
    output = bytearray()
    for _ in range(8):
        state = lcg(state)
        output.extend(struct.pack("<I", state))
    return bytes(output), state


def pairs(count: int, seed: int, profile: str) -> bytes:
    output = bytearray()
    basepoint = bytes([9]) + bytes(31)
    state = seed
    for index in range(count):
        if profile == "rfc7748":
            scalar, coordinate = RFC7748_PAIRS[(index + seed) % 2]
        elif profile == "generated":
            scalar, state = key_material(state)
            peer_scalar, state = key_material(state)
            coordinate = x25519(peer_scalar, basepoint)
        elif profile == "carry-heavy":
            scalar = bytearray(0xFF if (byte + index) % 3 else 0 for byte in range(32))
            peer_scalar = bytearray(
                0 if (byte + index) % 2 else 0xFF for byte in range(32)
            )
            state = lcg(state)
            scalar[index % 32] ^= state & 0xFF
            peer_scalar[(index * 7) % 32] ^= state >> 24
            coordinate = x25519(bytes(peer_scalar), basepoint)
            scalar = bytes(scalar)
        else:
            raise ValueError(f"unknown X25519 profile: {profile}")
        output.extend(scalar)
        output.extend(coordinate)
    return bytes(output)


def input_for(values: Mapping[str, object]) -> bytes:
    (repetitions, pair_count, seed), profile = profiled_parameters(
        values,
        ("repetitions", "pairs", "seed"),
        ("rfc7748", "generated", "carry-heavy"),
    )
    if not 1 <= repetitions <= 32 or not 1 <= pair_count <= 32:
        raise ValueError("x25519 repetitions or pairs is outside its limit")
    return struct.pack("<2I", repetitions, pair_count) + pairs(
        pair_count, seed, profile
    )


def output_for(data: bytes) -> bytes:
    header(data, "x25519")
    if len(data) < 72:
        raise ValueError("x25519 input is too short")
    repetitions, pair_count = struct.unpack_from("<2I", data)
    if (
        not 1 <= repetitions <= 32
        or not 1 <= pair_count <= 32
        or len(data) != 8 + pair_count * 64
    ):
        raise ValueError("x25519 input header is invalid")
    secrets = bytearray()
    for pair_index in range(pair_count):
        offset = 8 + pair_index * 64
        secrets.extend(
            x25519(data[offset : offset + 32], data[offset + 32 : offset + 64])
        )
    final_crc = zlib.crc32(secrets)
    aggregate = 0x5832_3535
    for pass_index in range(repetitions):
        aggregate = fold(aggregate, final_crc, pass_index)
    return record(25, aggregate, final_crc)
