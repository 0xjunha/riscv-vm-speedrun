#ifndef RVB_WORKLOAD_COMMON_H
#define RVB_WORKLOAD_COMMON_H

#include <stdint.h>

#define RVB_BAD_INPUT 1u
#define RVB_INTERNAL_ERROR 2u

static uint32_t rvb_read_u32(const uint8_t *bytes) {
    return (uint32_t)bytes[0] | ((uint32_t)bytes[1] << 8) |
           ((uint32_t)bytes[2] << 16) | ((uint32_t)bytes[3] << 24);
}

static uint32_t rvb_rotate_left(uint32_t value, uint32_t amount) {
    amount &= 31u;
    return amount == 0u ? value
                        : (value << amount) | (value >> (32u - amount));
}

/* Stable fold shared by the Python oracles: rol32(acc, 5) xor value, then add. */
static uint32_t rvb_fold(uint32_t accumulator, uint32_t value,
                         uint32_t index) {
    return (rvb_rotate_left(accumulator, 5u) ^ value) + 0x9e3779b9u +
           index;
}

static uint32_t rvb_crc32_update(uint32_t crc, const uint8_t *bytes,
                                 uint32_t length) {
    for (uint32_t i = 0; i < length; ++i) {
        crc ^= bytes[i];
        for (uint32_t bit = 0; bit < 8u; ++bit) {
            const uint32_t mask = 0u - (crc & 1u);
            crc = (crc >> 1) ^ (0xedb88320u & mask);
        }
    }
    return crc;
}

static uint32_t rvb_crc32_finish(uint32_t crc) { return ~crc; }

#endif
