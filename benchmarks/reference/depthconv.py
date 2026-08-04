"""Reference model for quantized depthwise convolution."""

from __future__ import annotations

import struct
import zlib
from collections.abc import Mapping

from .common import header, lcg, profiled_parameters, record, rotl

HEIGHT = 16
WIDTH = 16
CHANNELS = 8
FILTER = 3


def depth_input(repetitions: int, seed: int, profile: str = "balanced") -> bytes:
    state = seed
    activations = bytearray(HEIGHT * WIDTH * CHANNELS)
    for index in range(len(activations)):
        state = lcg(state)
        if profile == "balanced":
            value = ((state >> 24) & 0x7F) - 64
        elif profile == "sparse":
            value = ((state >> 27) - 16) if index % 19 == 0 else 0
        elif profile == "saturated":
            value = 127 if state & 1 else -128
        else:
            raise ValueError(f"unknown depthconv profile: {profile}")
        activations[index] = value & 0xFF

    weights = bytearray(FILTER * FILTER * CHANNELS)
    for index in range(len(weights)):
        state = lcg(state)
        if profile == "balanced":
            value = (state >> 27) - 16
        elif profile == "sparse":
            value = (state >> 28) - 8 if index % 5 == 0 else 0
        else:
            value = 15 if state & 2 else -16
        weights[index] = value & 0xFF

    biases = []
    multipliers = []
    shifts = []
    for channel in range(CHANNELS):
        state = lcg(state)
        if profile == "saturated":
            biases.append(0x1000 if channel % 2 else -0x1000)
        else:
            biases.append((state & 0x1FFF) - 0x1000)
        multipliers.append(1_152_862_902 + channel * 1_234_567)
        shifts.append(-8 + channel % 2)
    return b"".join(
        (
            struct.pack("<I", repetitions),
            activations,
            weights,
            struct.pack(f"<{CHANNELS}i", *biases),
            struct.pack(f"<{CHANNELS}i", *multipliers),
            struct.pack(f"<{CHANNELS}i", *shifts),
        )
    )


def input_for(values: Mapping[str, object]) -> bytes:
    (repetitions, seed), profile = profiled_parameters(
        values,
        ("repetitions", "seed"),
        ("balanced", "sparse", "saturated"),
    )
    if not 1 <= repetitions <= 32:
        raise ValueError("depthconv repetitions must be in 1..32")
    return depth_input(repetitions, seed, profile)


def output_for(data: bytes) -> bytes:
    header(data, "depthconv")
    repetitions = struct.unpack_from("<I", data)[0]
    activation_count = HEIGHT * WIDTH * CHANNELS
    weight_count = FILTER * FILTER * CHANNELS
    activation_offset = 4
    weight_offset = activation_offset + activation_count
    bias_offset = weight_offset + weight_count
    multiplier_offset = bias_offset + CHANNELS * 4
    shift_offset = multiplier_offset + CHANNELS * 4
    expected_size = shift_offset + CHANNELS * 4
    if len(data) != expected_size:
        raise ValueError("depthconv input has the wrong size")
    activations = data[activation_offset:weight_offset]
    weights = data[weight_offset:bias_offset]
    biases = struct.unpack_from(f"<{CHANNELS}i", data, bias_offset)
    multipliers = struct.unpack_from(f"<{CHANNELS}i", data, multiplier_offset)
    shifts = struct.unpack_from(f"<{CHANNELS}i", data, shift_offset)

    output = bytearray(activation_count)
    for out_y in range(HEIGHT):
        for out_x in range(WIDTH):
            for channel in range(CHANNELS):
                accumulator = biases[channel]
                for filter_y in range(FILTER):
                    in_y = out_y + filter_y - 1
                    if not 0 <= in_y < HEIGHT:
                        continue
                    for filter_x in range(FILTER):
                        in_x = out_x + filter_x - 1
                        if not 0 <= in_x < WIDTH:
                            continue
                        input_index = (in_y * WIDTH + in_x) * CHANNELS + channel
                        weight_index = (
                            filter_y * FILTER + filter_x
                        ) * CHANNELS + channel
                        input_value = struct.unpack(
                            "b", activations[input_index : input_index + 1]
                        )[0]
                        weight = struct.unpack(
                            "b", weights[weight_index : weight_index + 1]
                        )[0]
                        accumulator += weight * (input_value + 3)
                total_shift = 31 - shifts[channel]
                value = (
                    accumulator * multipliers[channel] + (1 << (total_shift - 1))
                ) >> total_shift
                value = min(127, max(-128, value - 2))
                output[(out_y * WIDTH + out_x) * CHANNELS + channel] = value & 0xFF
    checksum = zlib.crc32(output)
    aggregate = 0
    for pass_index in range(repetitions):
        aggregate ^= rotl(checksum, pass_index)
    return record(aggregate, checksum)
