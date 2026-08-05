"""Independent AES-256 model for the Embench Nettle AES workload."""

from __future__ import annotations

import struct
import zlib
from collections.abc import Mapping

from .common import generated_bytes, lcg, profiled_parameters, result


def _multiply(left: int, right: int) -> int:
    output = 0
    for _ in range(8):
        if right & 1:
            output ^= left
        left = (left << 1) ^ (0x11B if left & 0x80 else 0)
        right >>= 1
    return output & 0xFF


def _power(value: int, exponent: int) -> int:
    output = 1
    while exponent:
        if exponent & 1:
            output = _multiply(output, value)
        value = _multiply(value, value)
        exponent >>= 1
    return output


def _substitution(value: int) -> int:
    inverse = 0 if value == 0 else _power(value, 254)
    output = inverse
    for shift in range(1, 5):
        output ^= ((inverse << shift) | (inverse >> (8 - shift))) & 0xFF
    return output ^ 0x63


SBOX = tuple(_substitution(value) for value in range(256))


def _sub_word(word: int) -> int:
    return sum(SBOX[(word >> shift) & 0xFF] << shift for shift in (24, 16, 8, 0))


def _round_keys(key: bytes) -> tuple[int, ...]:
    words = list(struct.unpack(">8I", key))
    round_constant = 1
    while len(words) < 60:
        temporary = words[-1]
        if len(words) % 8 == 0:
            temporary = _sub_word(((temporary << 8) | (temporary >> 24)) & 0xFFFF_FFFF)
            temporary ^= round_constant << 24
            round_constant = _multiply(round_constant, 2)
        elif len(words) % 8 == 4:
            temporary = _sub_word(temporary)
        words.append(words[-8] ^ temporary)
    return tuple(words)


def _add_round_key(state: list[int], words: tuple[int, ...], round_index: int) -> None:
    for column in range(4):
        word = words[round_index * 4 + column]
        for row in range(4):
            state[column * 4 + row] ^= (word >> (24 - row * 8)) & 0xFF


def _shift_rows(state: list[int]) -> list[int]:
    return [
        state[((column + row) % 4) * 4 + row] for column in range(4) for row in range(4)
    ]


def _mix_columns(state: list[int]) -> None:
    for column in range(4):
        offset = column * 4
        a, b, c, d = state[offset : offset + 4]
        state[offset : offset + 4] = (
            _multiply(a, 2) ^ _multiply(b, 3) ^ c ^ d,
            a ^ _multiply(b, 2) ^ _multiply(c, 3) ^ d,
            a ^ b ^ _multiply(c, 2) ^ _multiply(d, 3),
            _multiply(a, 3) ^ b ^ c ^ _multiply(d, 2),
        )


def _encrypt_block(key: bytes, block: bytes) -> bytes:
    words = _round_keys(key)
    state = list(block)
    _add_round_key(state, words, 0)
    for round_index in range(1, 14):
        state = [SBOX[value] for value in state]
        state = _shift_rows(state)
        _mix_columns(state)
        _add_round_key(state, words, round_index)
    state = [SBOX[value] for value in state]
    state = _shift_rows(state)
    _add_round_key(state, words, 14)
    return bytes(state)


def input_for(values: Mapping[str, object]) -> bytes:
    (records, seed), profile = profiled_parameters(
        values, ("records", "seed"), ("random", "zero-heavy", "counter")
    )
    if not 1 <= records <= 64:
        raise ValueError("aes records is outside its limit")
    output = bytearray()
    state = seed
    for record in range(records):
        if profile == "random":
            key = generated_bytes(32, state)
            state = lcg(state)
            plaintext = generated_bytes(32, state)
        elif profile == "zero-heavy":
            key = bytearray(32)
            plaintext = bytearray(32)
            for index in range(0, 32, 5):
                state = lcg(state)
                key[index] = state >> 24
            for index in range(record % 7, 32, 7):
                state = lcg(state)
                plaintext[index] = state >> 24
            key = bytes(key)
            plaintext = bytes(plaintext)
        else:
            key = bytes((index * 17 + seed + record) & 0xFF for index in range(32))
            plaintext = b"".join(
                (record * 2 + block).to_bytes(16, "big") for block in range(2)
            )
        output.extend(key)
        output.extend(plaintext)
        state = lcg(state)
    return bytes(output)


def output_for(data: bytes) -> bytes:
    if not data or len(data) % 64 or len(data) // 64 > 64:
        raise ValueError("aes input has an invalid size")
    encrypted = bytearray()
    plaintext = bytearray()
    for offset in range(0, len(data), 64):
        key = data[offset : offset + 32]
        clear = data[offset + 32 : offset + 64]
        encrypted.extend(_encrypt_block(key, clear[:16]))
        encrypted.extend(_encrypt_block(key, clear[16:]))
        plaintext.extend(clear)
    return result(zlib.crc32(encrypted), zlib.crc32(plaintext))
