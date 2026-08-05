"""Reference model for the Embench SGLIB container workload."""

from __future__ import annotations

import struct
from collections.abc import Mapping

from .common import fold, lcg, profiled_parameters, result


def input_for(values: Mapping[str, object]) -> bytes:
    (count, seed), profile = profiled_parameters(
        values, ("count", "seed"), ("random", "clustered", "ordered")
    )
    if not 16 <= count <= 384:
        raise ValueError("sglib count is outside its limit")
    state = seed
    numbers = []
    for index in range(count):
        state = lcg(state)
        if profile == "random":
            value = state & 0xFFFF
        elif profile == "clustered":
            value = ((state >> 24) & 15) * 257 + index % 5
        else:
            value = index * 17
        numbers.append(value)
    return struct.pack(f"<{count}i", *numbers)


def output_for(data: bytes) -> bytes:
    if len(data) < 64 or len(data) % 4 or len(data) // 4 > 384:
        raise ValueError("sglib input has an invalid size")
    values = [item[0] for item in struct.iter_unpack("<i", data)]
    ordered_values = sorted(values)
    unique_values = sorted(set(values))

    ordered = 0x5347_4C49
    for index, value in enumerate(ordered_values):
        ordered = fold(ordered, value, index)
    index = len(values)
    for value in ordered_values:
        ordered = fold(ordered, value, index)
        index += 1
    for value in unique_values:
        ordered = fold(ordered, value, index)
        index += 1

    operations = 0x434F_4E54
    index = 0
    for value in values:
        operations = fold(operations, value, index)
        index += 1
    heap: list[int] = []
    for value in values:
        heap.append(value)
        child = len(heap) - 1
        while child > 0 and heap[child // 2] < heap[child]:
            parent = child // 2
            heap[parent], heap[child] = heap[child], heap[parent]
            child = parent
    heap_order = []
    while heap:
        heap_order.append(heap[0])
        heap[0] = heap[-1]
        heap.pop()
        parent = 0
        while True:
            largest = parent
            left = 2 * parent + 1
            right = left + 1
            if left < len(heap) and heap[largest] < heap[left]:
                largest = left
            if right < len(heap) and heap[largest] < heap[right]:
                largest = right
            if largest == parent:
                break
            heap[parent], heap[largest] = heap[largest], heap[parent]
            parent = largest
    for value in heap_order:
        operations = fold(operations, value, index)
        index += 1
    operations = fold(operations, len(unique_values), index)
    return result(ordered, operations)
