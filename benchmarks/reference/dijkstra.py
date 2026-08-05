"""Reference model for bounded Dijkstra shortest paths."""

from __future__ import annotations

import struct
from collections.abc import Mapping

from .common import MASK32, header, lcg, profiled_parameters, result, rotl, u32


def graph(nodes: int, density: int, seed: int, profile: str = "uniform") -> bytes:
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
            state = lcg(state)
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


def input_for(values: Mapping[str, object]) -> bytes:
    (nodes, sources, density, seed), profile = profiled_parameters(
        values,
        ("nodes", "sources", "density", "seed"),
        ("uniform", "clustered", "hub"),
    )
    if not 1 <= sources <= nodes:
        raise ValueError("dijkstra sources must be in 1..nodes")
    return struct.pack("<2I", nodes, sources) + graph(nodes, density, seed, profile)


def output_for(data: bytes) -> bytes:
    header(data, "dijkstra")
    nodes, sources = struct.unpack_from("<2I", data)
    matrix = struct.unpack_from(f"<{nodes * nodes}H", data, 8)
    total = 0
    folded = 0
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
            total = u32(total + distance)
            folded ^= rotl(distance, node + source)
    return result(total, folded)
