"""Reference model for Embench integer LU decomposition."""

from __future__ import annotations

import struct
from collections.abc import Mapping

from .common import fold, lcg, profiled_parameters, result

SIZE = 20
RECORD_VALUES = SIZE * SIZE + SIZE


def _matrix(state: int, profile: str) -> tuple[list[list[int]], list[int], int]:
    lower = [[0] * SIZE for _ in range(SIZE)]
    upper = [[0] * SIZE for _ in range(SIZE)]
    solution = []
    for row in range(SIZE):
        lower[row][row] = 1
        state = lcg(state)
        solution.append(
            1 + state % 3 if profile == "positive" else (state % 7) - 3 or 1
        )
        for column in range(row):
            state = lcg(state)
            if profile == "banded" and row - column > 3:
                continue
            lower[row][column] = state % 3 if profile == "positive" else (state % 5) - 2
        for column in range(row, SIZE):
            state = lcg(state)
            if profile == "banded" and column - row > 3:
                continue
            if column == row:
                upper[row][column] = 23 + row + (state & 7)
            elif profile == "positive":
                upper[row][column] = state % 5
            else:
                upper[row][column] = (state % 9) - 4
    matrix = [
        [
            sum(lower[row][k] * upper[k][column] for k in range(SIZE))
            for column in range(SIZE)
        ]
        for row in range(SIZE)
    ]
    vector = [
        sum(matrix[row][column] * solution[column] for column in range(SIZE))
        for row in range(SIZE)
    ]
    return matrix, vector, state


def input_for(values: Mapping[str, object]) -> bytes:
    (systems, seed), profile = profiled_parameters(
        values, ("systems", "seed"), ("dense", "banded", "positive")
    )
    if not 1 <= systems <= 8:
        raise ValueError("ud systems is outside its limit")
    output = bytearray()
    state = seed
    for _ in range(systems):
        matrix, vector, state = _matrix(state, profile)
        flat = [value for row in matrix for value in row]
        output.extend(struct.pack(f"<{RECORD_VALUES}i", *flat, *vector))
    return bytes(output)


def _c_div(numerator: int, denominator: int) -> int:
    if denominator == 0:
        raise ValueError("ud input produces a zero pivot")
    quotient = abs(numerator) // abs(denominator)
    return -quotient if (numerator < 0) != (denominator < 0) else quotient


def _solve(
    matrix: list[list[int]], vector: list[int]
) -> tuple[list[int], list[list[int]]]:
    y = [0] * SIZE
    solution = [0] * SIZE
    for i in range(SIZE - 1):
        for j in range(i + 1, SIZE):
            value = matrix[j][i]
            if i:
                for k in range(i):
                    value -= matrix[j][k] * matrix[k][i]
            matrix[j][i] = _c_div(value, matrix[i][i])
        for j in range(i + 1, SIZE):
            value = matrix[i + 1][j]
            for k in range(i + 1):
                value -= matrix[i + 1][k] * matrix[k][j]
            matrix[i + 1][j] = value
    y[0] = vector[0]
    for i in range(1, SIZE):
        value = vector[i]
        for j in range(i):
            value -= matrix[i][j] * y[j]
        y[i] = value
    solution[-1] = _c_div(y[-1], matrix[-1][-1])
    for i in range(SIZE - 2, -1, -1):
        value = y[i]
        for j in range(i + 1, SIZE):
            value -= matrix[i][j] * solution[j]
        solution[i] = _c_div(value, matrix[i][i])
    return solution, matrix


def output_for(data: bytes) -> bytes:
    record_size = RECORD_VALUES * 4
    if not data or len(data) % record_size or len(data) // record_size > 8:
        raise ValueError("ud input has an invalid size")
    solutions = 0x5544_4C55
    factors = 0x4445_434F
    observation = 0
    for unpacked in struct.iter_unpack(f"<{RECORD_VALUES}i", data):
        matrix = [list(unpacked[row * SIZE : (row + 1) * SIZE]) for row in range(SIZE)]
        vector = list(unpacked[SIZE * SIZE :])
        solved, matrix = _solve(matrix, vector)
        for value in solved:
            solutions = fold(solutions, value, observation)
            observation += 1
        for row in range(SIZE):
            for column in range(SIZE):
                factors = fold(factors, matrix[row][column], row * SIZE + column)
    return result(solutions, factors)
