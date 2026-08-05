"""Reference model for the Heatshrink telemetry workload."""

from __future__ import annotations

import struct
import zlib
from collections.abc import Mapping

from .common import lcg, profiled_parameters, result, rotl

TELEMETRY_FORMAT = "<IHHIiii"
TELEMETRY_RECORD_SIZE = struct.calcsize(TELEMETRY_FORMAT)
MAX_PAYLOAD = 16 * 1024


def telemetry(records: int, seed: int, profile: str = "nominal") -> bytes:
    output = bytearray()
    state = seed
    for sequence in range(records):
        state = lcg(state)
        if profile == "nominal":
            device = 0x1200 | ((sequence // 64) & 0xFF)
            timestamp = 1_700_000_000 + sequence * 20
            temperature = 20_000 + sequence % 48 + (state & 31)
            pressure = 100_000 + ((state >> 8) & 255)
            vibration = ((state >> 20) & 31) - 16
        elif profile == "steady":
            device = 0x1201
            timestamp = 1_700_000_000 + sequence * 20
            temperature = 21_500 + sequence % 4
            pressure = 100_800 + sequence % 8
            vibration = sequence % 3 - 1
        elif profile == "bursty":
            device = 0x1200 | (state & 0xFF)
            timestamp = 1_700_000_000 + sequence * 20 + ((state >> 8) & 15)
            temperature = 12_000 + ((state >> 12) & 0x3FFF)
            pressure = 80_000 + ((state >> 4) & 0x7FFF)
            vibration = ((state >> 22) & 0x3FF) - 512
        else:
            raise ValueError(f"unknown telemetry profile: {profile}")
        output.extend(
            struct.pack(
                TELEMETRY_FORMAT,
                0x314D_4C54,
                device,
                sequence & 0xFFFF,
                timestamp,
                temperature,
                pressure,
                vibration,
            )
        )
    return bytes(output)


def encode(data: bytes) -> bytes:
    """Independently encode the default W=8, L=4 Heatshrink bitstream."""

    source = bytes(256) + data
    output = bytearray()
    current = 0
    used = 0

    def write_bits(value: int, count: int) -> None:
        nonlocal current, used
        for shift in range(count - 1, -1, -1):
            current = (current << 1) | ((value >> shift) & 1)
            used += 1
            if used == 8:
                output.append(current)
                current = 0
                used = 0

    position = 256
    while position < len(source):
        start = position - 256
        maximum = min(16, len(source) - position)
        best_position = position
        best_length = 0
        for candidate in range(position - 1, start - 1, -1):
            if source[candidate] != source[position]:
                continue
            length = 1
            while (
                length < maximum
                and source[candidate + length] == source[position + length]
            ):
                length += 1
            if length > best_length:
                best_position = candidate
                best_length = length
                if length == maximum:
                    break
        if best_length > 1:
            write_bits(0, 1)
            write_bits(position - best_position - 1, 8)
            write_bits(best_length - 1, 4)
            position += best_length
        else:
            write_bits(1, 1)
            write_bits(source[position], 8)
            position += 1

    if used:
        output.append(current << (8 - used))
    return bytes(output)


def input_for(values: Mapping[str, object]) -> bytes:
    (records, seed), profile = profiled_parameters(
        values, ("records", "seed"), ("nominal", "steady", "bursty")
    )
    if records > MAX_PAYLOAD // TELEMETRY_RECORD_SIZE:
        raise ValueError("heatshrink records exceed the 16 KiB payload limit")
    data = telemetry(records, seed, profile)
    return data


def output_for(data: bytes) -> bytes:
    if len(data) > MAX_PAYLOAD:
        raise ValueError("heatshrink input exceeds the 16 KiB payload limit")
    encoded = encode(data)
    decoded_summary = zlib.crc32(data) ^ rotl(len(encoded), 16)
    return result(zlib.crc32(encoded), decoded_summary)
