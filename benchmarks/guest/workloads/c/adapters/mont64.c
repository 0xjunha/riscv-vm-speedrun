#include "rvb_workload_common.h"

#include <stdint.h>

uint64_t montmul(uint64_t a, uint64_t b, uint64_t modulus,
                 uint64_t modulus_inverse);
uint64_t modul64(uint64_t high, uint64_t low, uint64_t modulus);

static uint64_t read_u64(const uint8_t *bytes) {
    return (uint64_t)rvb_read_u32(bytes) |
           ((uint64_t)rvb_read_u32(bytes + 4) << 32);
}

uint32_t rvb_mont64(const uint8_t *input, uint32_t input_len,
                    uint32_t out[2]) {
    const uint32_t record_size = 32u;
    if (input_len == 0u || input_len % record_size != 0u ||
        input_len / record_size > 512u) {
        return RVB_BAD_INPUT;
    }

    uint32_t products = 0x4d4f4e54u;
    uint32_t remainders = 0x36345256u;
    for (uint32_t offset = 0u, index = 0u; offset < input_len;
         offset += record_size, ++index) {
        const uint64_t a = read_u64(input + offset);
        const uint64_t b = read_u64(input + offset + 8u);
        const uint64_t modulus = read_u64(input + offset + 16u);
        const uint64_t inverse = read_u64(input + offset + 24u);
        if (modulus < 3u || (modulus & 1u) == 0u || a >= modulus ||
            b >= modulus || modulus * inverse != UINT64_MAX) {
            return RVB_BAD_INPUT;
        }

        const uint64_t product = montmul(a, b, modulus, inverse);
        const uint64_t remainder = modul64(a, b, modulus);
        products = rvb_fold(products,
                            (uint32_t)product ^ (uint32_t)(product >> 32),
                            index);
        remainders = rvb_fold(
            remainders,
            (uint32_t)remainder ^ (uint32_t)(remainder >> 32), index);
    }

    out[0] = products;
    out[1] = remainders;
    return 0u;
}
