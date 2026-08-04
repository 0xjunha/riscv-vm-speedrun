"""Independent input and expected-output models for the public workloads."""

from __future__ import annotations

import hashlib
import struct
import zlib
from collections.abc import Mapping

MASK32 = 0xFFFF_FFFF
OUTPUT_MAGIC = 0x3142_5652  # bytes: RVB1
DEPTH_HEIGHT = 16
DEPTH_WIDTH = 16
DEPTH_CHANNELS = 8
DEPTH_FILTER = 3
TELEMETRY_FORMAT = "<IHHIiii"
TELEMETRY_RECORD_SIZE = struct.calcsize(TELEMETRY_FORMAT)
HEATSHRINK_MAX_PAYLOAD = 16 * 1024
QRCODE_VECTORS = {
    "95dea1e7f058ed0a69642b2182868a8b327ece4233e8101ed1e6d2bf4dec67e2": {
        "dark_modules": 1192,
        "auxiliary": 0x3EDE_9F7D,
    },
    "89e8eab7b14478f2cf25ee1fb6ca8a451d304e49e575e7f745d952205dbefcb4": {
        "dark_modules": 444,
        "auxiliary": 0xD4CC_255A,
    },
    "0a5da553401adc7aac0e2a218fb0b12654f3e317c9b56005d0270278994f71ef": {
        "dark_modules": 4756,
        "auxiliary": 0x3AA8_FCA1,
    },
}
X25519_RFC7748_PAIRS = (
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


def _u32(value: int) -> int:
    return value & MASK32


def _rotl(value: int, shift: int) -> int:
    shift &= 31
    return _u32((value << shift) | (value >> ((32 - shift) & 31)))


def _rotr(value: int, shift: int) -> int:
    shift &= 31
    return _u32((value >> shift) | (value << ((32 - shift) & 31)))


def _parameter(parameters: Mapping[str, object], name: str) -> int:
    value = parameters.get(name)
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not 0 <= value <= MASK32
    ):
        raise ValueError(f"{name} must be an unsigned 32-bit integer")
    return value


def _parameters(
    parameters: Mapping[str, object], names: tuple[str, ...]
) -> tuple[int, ...]:
    if set(parameters) != set(names):
        raise ValueError(f"parameters must be {', '.join(names)}")
    return tuple(_parameter(parameters, name) for name in names)


def _profiled_parameters(
    parameters: Mapping[str, object],
    names: tuple[str, ...],
    profiles: tuple[str, ...],
) -> tuple[tuple[int, ...], str]:
    expected = set(names)
    if set(parameters) not in (expected, expected | {"profile"}):
        raise ValueError(
            f"parameters must be {', '.join(names)}, with optional profile"
        )
    profile = parameters.get("profile", profiles[0])
    if not isinstance(profile, str) or profile not in profiles:
        raise ValueError(f"profile must be one of {', '.join(profiles)}")
    return tuple(_parameter(parameters, name) for name in names), profile


def _lcg(state: int) -> int:
    return _u32(state * 1_664_525 + 1_013_904_223)


def _fold(accumulator: int, value: int, index: int) -> int:
    return _u32((_rotl(accumulator, 5) ^ value) + 0x9E37_79B9 + index)


def _generated_bytes(length: int, seed: int) -> bytes:
    output = bytearray(length)
    state = seed
    for index in range(length):
        state = _lcg(state)
        output[index] = state >> 24
    return bytes(output)


def _littlefs_operations(count: int, seed: int, profile: str) -> bytes:
    words = []
    state = seed
    for index in range(count):
        state = _lcg(state)
        second = state
        state = _lcg(state)
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


def _x25519(scalar: bytes, coordinate: bytes) -> bytes:
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


def _x25519_key_material(state: int) -> tuple[bytes, int]:
    output = bytearray()
    for _ in range(8):
        state = _lcg(state)
        output.extend(struct.pack("<I", state))
    return bytes(output), state


def _x25519_pairs(count: int, seed: int, profile: str) -> bytes:
    output = bytearray()
    basepoint = bytes([9]) + bytes(31)
    state = seed
    for index in range(count):
        if profile == "rfc7748":
            scalar, coordinate = X25519_RFC7748_PAIRS[(index + seed) % 2]
        elif profile == "generated":
            scalar, state = _x25519_key_material(state)
            peer_scalar, state = _x25519_key_material(state)
            coordinate = _x25519(peer_scalar, basepoint)
        elif profile == "carry-heavy":
            scalar = bytearray(0xFF if (byte + index) % 3 else 0 for byte in range(32))
            peer_scalar = bytearray(
                0 if (byte + index) % 2 else 0xFF for byte in range(32)
            )
            state = _lcg(state)
            scalar[index % 32] ^= state & 0xFF
            peer_scalar[(index * 7) % 32] ^= state >> 24
            coordinate = _x25519(bytes(peer_scalar), basepoint)
            scalar = bytes(scalar)
        else:
            raise ValueError(f"unknown X25519 profile: {profile}")
        output.extend(scalar)
        output.extend(coordinate)
    return bytes(output)


def _firmware_payload(length: int, seed: int, profile: str) -> bytes:
    if profile == "pseudorandom":
        return _generated_bytes(length, seed)
    if profile == "repeated-pages":
        if length == 0:
            return b""
        page = _generated_bytes(min(length, 256), seed)
        return (page * ((length + len(page) - 1) // len(page)))[:length]
    if profile == "sparse-flash":
        output = bytearray([0xFF]) * length
        state = seed
        for offset in range(0, length, 256):
            for index in range(offset, min(offset + 16, length)):
                state = _lcg(state)
                output[index] = state >> 24
        return bytes(output)
    raise ValueError(f"unknown firmware profile: {profile}")


def _telemetry(records: int, seed: int, profile: str = "nominal") -> bytes:
    output = bytearray()
    state = seed
    for sequence in range(records):
        state = _lcg(state)
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


def _depth_input(repetitions: int, seed: int, profile: str = "balanced") -> bytes:
    state = seed
    activations = bytearray(DEPTH_HEIGHT * DEPTH_WIDTH * DEPTH_CHANNELS)
    for index in range(len(activations)):
        state = _lcg(state)
        if profile == "balanced":
            value = ((state >> 24) & 0x7F) - 64
        elif profile == "sparse":
            value = ((state >> 27) - 16) if index % 19 == 0 else 0
        elif profile == "saturated":
            value = 127 if state & 1 else -128
        else:
            raise ValueError(f"unknown depthconv profile: {profile}")
        activations[index] = value & 0xFF

    weights = bytearray(DEPTH_FILTER * DEPTH_FILTER * DEPTH_CHANNELS)
    for index in range(len(weights)):
        state = _lcg(state)
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
    for channel in range(DEPTH_CHANNELS):
        state = _lcg(state)
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
            struct.pack(f"<{DEPTH_CHANNELS}i", *biases),
            struct.pack(f"<{DEPTH_CHANNELS}i", *multipliers),
            struct.pack(f"<{DEPTH_CHANNELS}i", *shifts),
        )
    )


def _graph(nodes: int, density: int, seed: int, profile: str = "uniform") -> bytes:
    if not 2 <= nodes <= 64:
        raise ValueError("dijkstra nodes must be in 2..64")
    if density > 100:
        raise ValueError("dijkstra density must not exceed 100")
    matrix = [0] * (nodes * nodes)
    state = seed
    for source in range(nodes):
        for target in range(nodes):
            if source == target:
                continue
            state = _lcg(state)
            ring = target == (source + 1) % nodes or source == (target + 1) % nodes
            if profile == "uniform":
                selected = state % 100 < density
            elif profile == "clustered":
                group_size = max(2, nodes // 4)
                same_group = source // group_size == target // group_size
                threshold = density if same_group else max(1, density // 12)
                selected = state % 100 < threshold
            elif profile == "hub":
                selected = source == 0 or target == 0 or state % 100 < density
            else:
                raise ValueError(f"unknown graph profile: {profile}")
            if ring or selected:
                matrix[source * nodes + target] = 1 + (state >> 16) % 255
    return struct.pack(f"<{len(matrix)}H", *matrix)


def _records(count: int, seed: int, profile: str = "uniform") -> bytes:
    values = []
    state = seed
    for index in range(count):
        state = _lcg(state)
        if profile == "uniform":
            key = (state ^ (state >> 16)) & 0xFFFF
        elif profile == "ascending":
            key = index
        elif profile == "duplicate-heavy":
            key = (state >> 28) & 0xF
        else:
            raise ValueError(f"unknown record profile: {profile}")
        state = _lcg(state)
        values.extend((key, state))
    return struct.pack(f"<{len(values)}I", *values)


def _provisioning_payload(length: int, seed: int) -> bytes:
    output = bytearray()
    state = seed
    sequence = 0
    while len(output) < length:
        state = _lcg(state)
        line = (
            f"device=rv-{state:08x};batch={sequence // 8:04x};"
            f"counter={sequence:05d};fw=2026.07;scope=sensor\n"
        ).encode()
        output.extend(line)
        sequence += 1
    return bytes(output[:length])


def input_for(workload: str, parameters: Mapping[str, object]) -> bytes:
    """Encode one authored case as the guest's little-endian input."""

    if workload == "tiny":
        words = _parameters(parameters, ("a", "b"))
        return struct.pack("<2I", *words)
    if workload == "arithmetic":
        words = _parameters(parameters, ("iterations", "x", "y"))
        return struct.pack("<3I", *words)
    if workload == "streaming":
        passes, count, seed = _parameters(parameters, ("passes", "count", "seed"))
        if count > 1024:
            raise ValueError("streaming count must not exceed 1024")
        values = []
        state = seed
        for _ in range(count):
            state = _lcg(state)
            values.append(state)
        return struct.pack(f"<{len(values) + 2}I", passes, count, *values)
    if workload == "sha256":
        (length, seed), profile = _profiled_parameters(
            parameters,
            ("length", "seed"),
            ("pseudorandom", "repeated-pages", "sparse-flash"),
        )
        if length > 512 * 1024:
            raise ValueError("sha256 length must not exceed 512 KiB")
        return struct.pack("<I", length) + _firmware_payload(length, seed, profile)
    if workload == "heatshrink":
        (records, seed), profile = _profiled_parameters(
            parameters, ("records", "seed"), ("nominal", "steady", "bursty")
        )
        if records > HEATSHRINK_MAX_PAYLOAD // TELEMETRY_RECORD_SIZE:
            raise ValueError("heatshrink records exceed the 16 KiB payload limit")
        data = _telemetry(records, seed, profile)
        return struct.pack("<I", len(data)) + data
    if workload == "depthconv":
        (repetitions, seed), profile = _profiled_parameters(
            parameters,
            ("repetitions", "seed"),
            ("balanced", "sparse", "saturated"),
        )
        if not 1 <= repetitions <= 32:
            raise ValueError("depthconv repetitions must be in 1..32")
        return _depth_input(repetitions, seed, profile)
    if workload == "dijkstra":
        (nodes, sources, density, seed), profile = _profiled_parameters(
            parameters,
            ("nodes", "sources", "density", "seed"),
            ("uniform", "clustered", "hub"),
        )
        if not 1 <= sources <= nodes:
            raise ValueError("dijkstra sources must be in 1..nodes")
        return struct.pack("<2I", nodes, sources) + _graph(
            nodes, density, seed, profile
        )
    if workload == "sort_records":
        (count, passes, seed), profile = _profiled_parameters(
            parameters,
            ("count", "passes", "seed"),
            ("uniform", "ascending", "duplicate-heavy"),
        )
        if not 2 <= count <= 2048 or not 1 <= passes <= 16:
            raise ValueError("sort_records count or passes is outside its limit")
        return struct.pack("<2I", count, passes) + _records(count, seed, profile)
    if workload == "qrcode":
        length, seed = _parameters(parameters, ("length", "seed"))
        if length > 1024:
            raise ValueError("qrcode length must not exceed 1024")
        return struct.pack("<I", length) + _provisioning_payload(length, seed)
    if workload == "littlefs":
        (repetitions, operations, seed), profile = _profiled_parameters(
            parameters,
            ("repetitions", "operations", "seed"),
            ("mixed", "append-heavy", "metadata-churn"),
        )
        if not 1 <= repetitions <= 16 or not 1 <= operations <= 96:
            raise ValueError("littlefs repetitions or operations is outside its limit")
        return struct.pack("<2I", repetitions, operations) + _littlefs_operations(
            operations, seed, profile
        )
    if workload == "x25519":
        (repetitions, pairs, seed), profile = _profiled_parameters(
            parameters,
            ("repetitions", "pairs", "seed"),
            ("rfc7748", "generated", "carry-heavy"),
        )
        if not 1 <= repetitions <= 32 or not 1 <= pairs <= 32:
            raise ValueError("x25519 repetitions or pairs is outside its limit")
        return struct.pack("<2I", repetitions, pairs) + _x25519_pairs(
            pairs, seed, profile
        )
    raise ValueError(f"unknown workload: {workload}")


def _words(data: bytes) -> tuple[int, ...]:
    if len(data) % 4:
        raise ValueError("workload input length must be a multiple of four")
    return tuple(value[0] for value in struct.iter_unpack("<I", data))


def _record(family: int, result: int, auxiliary: int) -> bytes:
    return struct.pack("<IIII", OUTPUT_MAGIC, family, _u32(result), _u32(auxiliary))


def _heatshrink_encode(data: bytes) -> bytes:
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


def _depth_output(data: bytes) -> bytes:
    repetitions = struct.unpack_from("<I", data)[0]
    activation_count = DEPTH_HEIGHT * DEPTH_WIDTH * DEPTH_CHANNELS
    weight_count = DEPTH_FILTER * DEPTH_FILTER * DEPTH_CHANNELS
    activation_offset = 4
    weight_offset = activation_offset + activation_count
    bias_offset = weight_offset + weight_count
    multiplier_offset = bias_offset + DEPTH_CHANNELS * 4
    shift_offset = multiplier_offset + DEPTH_CHANNELS * 4
    expected_size = shift_offset + DEPTH_CHANNELS * 4
    if len(data) != expected_size:
        raise ValueError("depthconv input has the wrong size")
    activations = data[activation_offset:weight_offset]
    weights = data[weight_offset:bias_offset]
    biases = struct.unpack_from(f"<{DEPTH_CHANNELS}i", data, bias_offset)
    multipliers = struct.unpack_from(f"<{DEPTH_CHANNELS}i", data, multiplier_offset)
    shifts = struct.unpack_from(f"<{DEPTH_CHANNELS}i", data, shift_offset)

    output = bytearray(activation_count)
    for out_y in range(DEPTH_HEIGHT):
        for out_x in range(DEPTH_WIDTH):
            for channel in range(DEPTH_CHANNELS):
                accumulator = biases[channel]
                for filter_y in range(DEPTH_FILTER):
                    in_y = out_y + filter_y - 1
                    if not 0 <= in_y < DEPTH_HEIGHT:
                        continue
                    for filter_x in range(DEPTH_FILTER):
                        in_x = out_x + filter_x - 1
                        if not 0 <= in_x < DEPTH_WIDTH:
                            continue
                        input_index = (
                            in_y * DEPTH_WIDTH + in_x
                        ) * DEPTH_CHANNELS + channel
                        weight_index = (
                            filter_y * DEPTH_FILTER + filter_x
                        ) * DEPTH_CHANNELS + channel
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
                output[(out_y * DEPTH_WIDTH + out_x) * DEPTH_CHANNELS + channel] = (
                    value & 0xFF
                )
    checksum = zlib.crc32(output)
    aggregate = 0
    for pass_index in range(repetitions):
        aggregate ^= _rotl(checksum, pass_index)
    return _record(11, aggregate, checksum)


def _dijkstra_output(data: bytes) -> bytes:
    nodes, sources = struct.unpack_from("<2I", data)
    matrix = struct.unpack_from(f"<{nodes * nodes}H", data, 8)
    total = 0
    fold = 0
    for source in range(sources):
        distances = [MASK32] * nodes
        visited = [False] * nodes
        distances[source] = 0
        for _ in range(nodes):
            selected = nodes
            minimum = MASK32
            for node in range(nodes):
                if not visited[node] and distances[node] < minimum:
                    selected = node
                    minimum = distances[node]
            if selected == nodes:
                break
            visited[selected] = True
            for target in range(nodes):
                weight = matrix[selected * nodes + target]
                if not weight or visited[target]:
                    continue
                candidate = min(MASK32, minimum + weight)
                distances[target] = min(distances[target], candidate)
        for node, distance in enumerate(distances):
            total = _u32(total + distance)
            fold ^= _rotl(distance, node + source)
    return _record(13, total, fold)


def _sort_output(data: bytes) -> bytes:
    count, passes = struct.unpack_from("<2I", data)
    words = struct.unpack_from(f"<{count * 2}I", data, 8)
    aggregate = 0
    final_fold = 0
    for pass_index in range(passes):
        mask = _u32(pass_index * 0x9E37_79B9)
        records = sorted(
            ((words[index * 2] ^ mask, words[index * 2 + 1]) for index in range(count)),
            key=lambda record: record[0],
        )
        fold = 0x811C_9DC5
        for key, value in records:
            fold = _rotl(fold, 5) ^ key
            fold = _u32(fold * 0x0100_0193) ^ value
        aggregate ^= _rotl(fold, pass_index)
        final_fold = fold
    return _record(15, aggregate, final_fold)


def _littlefs_output(data: bytes) -> bytes:
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
    operations = tuple(words[index : index + 4] for index in range(0, len(words), 4))
    files: dict[int, bytes] = {}
    trace = 0x4C46_5332
    for index, (kind_word, file_word, second, third) in enumerate(operations):
        kind = kind_word % 5
        file_id = file_word & 15
        if kind <= 1:
            length = 1 + (second & 63)
            payload = _generated_bytes(length, third)
            files[file_id] = payload if kind == 0 else files.get(file_id, b"") + payload
            event = (
                (kind << 28) ^ (file_id << 24) ^ length ^ _rotl(zlib.crc32(payload), 1)
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
                    ^ _rotl(zlib.crc32(actual), 1)
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
        trace = _fold(trace, event, index)

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
        aggregate = _fold(aggregate, final_summary, pass_index)
    return _record(23, aggregate, final_summary)


def _x25519_output(data: bytes) -> bytes:
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
    for pair in range(pair_count):
        offset = 8 + pair * 64
        secrets.extend(
            _x25519(data[offset : offset + 32], data[offset + 32 : offset + 64])
        )
    final_crc = zlib.crc32(secrets)
    aggregate = 0x5832_3535
    for pass_index in range(repetitions):
        aggregate = _fold(aggregate, final_crc, pass_index)
    return _record(25, aggregate, final_crc)


def output_for(workload: str, data: bytes) -> bytes:
    """Compute the guest's result without executing guest code."""

    if workload == "tiny":
        words = _words(data)
        if len(words) != 2:
            raise ValueError("tiny input must contain two words")
        result = _rotl(words[0], 5) ^ _rotr(words[1], 3) ^ 0x7469_6E79
        return _record(1, result, len(data))

    if workload == "arithmetic":
        words = _words(data)
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
        words = _words(data)
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
                xor ^= _rotl(value, index + pass_index)
                weighted = _u32(weighted + (value ^ stride))
                stride = _u32(stride + 0x9E37_79B9)
        return _record(5, total ^ xor, weighted)

    if len(data) < 4:
        raise ValueError(f"{workload} input is missing its header")
    length = struct.unpack_from("<I", data)[0]
    if workload == "sha256":
        payload = data[4:]
        if len(payload) != length:
            raise ValueError("sha256 input length is invalid")
        digest = hashlib.sha256(payload).digest()
        return _record(
            7,
            int.from_bytes(digest[:4], "big"),
            int.from_bytes(digest[-4:], "big"),
        )
    if workload == "heatshrink":
        payload = data[4:]
        if len(payload) != length:
            raise ValueError("heatshrink input length is invalid")
        encoded = _heatshrink_encode(payload)
        auxiliary = zlib.crc32(payload) ^ _rotl(len(encoded), 16)
        return _record(9, zlib.crc32(encoded), auxiliary)
    if workload == "depthconv":
        return _depth_output(data)
    if workload == "dijkstra":
        return _dijkstra_output(data)
    if workload == "sort_records":
        return _sort_output(data)
    if workload == "qrcode":
        if len(data[4:]) != length:
            raise ValueError("qrcode input length is invalid")
        vector = QRCODE_VECTORS.get(hashlib.sha256(data).hexdigest())
        if vector is None:
            raise ValueError("qrcode input does not match its known-answer vector")
        return _record(
            17,
            vector["dark_modules"],
            vector["auxiliary"],
        )
    if workload == "littlefs":
        return _littlefs_output(data)
    if workload == "x25519":
        return _x25519_output(data)
    raise ValueError(f"unknown workload: {workload}")
