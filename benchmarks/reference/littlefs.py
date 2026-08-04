"""Reference model for the littlefs operation-trace workload."""

from __future__ import annotations

import struct
import zlib
from collections.abc import Mapping

from .common import (
    MASK32,
    fold,
    generated_bytes,
    header,
    lcg,
    profiled_parameters,
    record,
    rotl,
)


def operations(count: int, seed: int, profile: str) -> bytes:
    words = []
    state = seed
    for index in range(count):
        state = lcg(state)
        second = state
        state = lcg(state)
        third = state
        if profile == "mixed":
            phase = index % 5
            source = (index // 5) & 7
            destination = source + 8
            kind = phase
            file_id = destination if phase == 4 else source
        elif profile == "append-heavy":
            phase = index % 4
            file_id = (index // 4) & 7
            kind = (0, 1, 1, 2)[phase]
            destination = 0
        elif profile == "metadata-churn":
            phase = index % 6
            source = (index // 6) & 7
            destination = source + 8
            kind = (0, 0, 3, 2, 4, 2)[phase]
            file_id = destination if phase in (1, 3, 4, 5) else source
        else:
            raise ValueError(f"unknown littlefs profile: {profile}")
        offset_or_destination = destination if kind == 3 else second
        words.extend((kind, file_id, offset_or_destination, third))
    return struct.pack(f"<{len(words)}I", *words)


def input_for(values: Mapping[str, object]) -> bytes:
    (repetitions, operation_count, seed), profile = profiled_parameters(
        values,
        ("repetitions", "operations", "seed"),
        ("mixed", "append-heavy", "metadata-churn"),
    )
    if not 1 <= repetitions <= 16 or not 1 <= operation_count <= 96:
        raise ValueError("littlefs repetitions or operations is outside its limit")
    return struct.pack("<2I", repetitions, operation_count) + operations(
        operation_count, seed, profile
    )


def output_for(data: bytes) -> bytes:
    header(data, "littlefs")
    if len(data) < 24 or len(data) % 4:
        raise ValueError("littlefs input has an invalid size")
    repetitions, operation_count = struct.unpack_from("<2I", data)
    if (
        not 1 <= repetitions <= 16
        or not 1 <= operation_count <= 96
        or len(data) != 8 + operation_count * 16
    ):
        raise ValueError("littlefs input header is invalid")
    words = struct.unpack_from(f"<{operation_count * 4}I", data, 8)
    trace_operations = tuple(
        words[index : index + 4] for index in range(0, len(words), 4)
    )
    files: dict[int, bytes] = {}
    trace = 0x4C46_5332
    for index, (kind_word, file_word, second, third) in enumerate(trace_operations):
        kind = kind_word % 5
        file_id = file_word & 15
        if kind <= 1:
            length = 1 + (second & 63)
            payload = generated_bytes(length, third)
            files[file_id] = payload if kind == 0 else files.get(file_id, b"") + payload
            event = (
                (kind << 28) ^ (file_id << 24) ^ length ^ rotl(zlib.crc32(payload), 1)
            )
        elif kind == 2:
            if file_id not in files:
                event = (2 << 28) ^ (file_id << 24) ^ MASK32
            else:
                content = files[file_id]
                offset = second % (len(content) + 1)
                actual = content[offset : offset + 1 + (third & 63)]
                event = (
                    (2 << 28)
                    ^ (file_id << 24)
                    ^ offset
                    ^ len(actual)
                    ^ rotl(zlib.crc32(actual), 1)
                )
        elif kind == 3:
            destination = second & 15
            present = file_id in files
            if present and file_id != destination:
                files[destination] = files.pop(file_id)
            event = (
                (3 << 28)
                ^ (file_id << 24)
                ^ (destination << 20)
                ^ (0x1357_9BDF if present else 0x2468_ACE0)
            )
        else:
            present = file_id in files
            if present:
                del files[file_id]
            event = (
                (4 << 28) ^ (file_id << 24) ^ (0x1357_9BDF if present else 0x2468_ACE0)
            )
        trace = fold(trace, event, index)

    state = bytearray()
    for file_id in range(16):
        if file_id in files:
            content = files[file_id]
            state.append(file_id)
            state.extend(struct.pack("<I", len(content)))
            state.extend(content)
    final_summary = trace ^ zlib.crc32(state)
    aggregate = 0x4C46_5332
    for pass_index in range(repetitions):
        aggregate = fold(aggregate, final_summary, pass_index)
    return record(23, aggregate, final_summary)
