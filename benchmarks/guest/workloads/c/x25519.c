/* Project-owned adapter around the pinned upstream Monocypher workload. */

#include "rvb_workload_common.h"
#include "rvb_c_workloads.h"

#include "monocypher.h"

#include <stdint.h>

uint32_t rvb_x25519(const uint8_t *input, uint32_t input_len,
                    uint32_t out[2]) {
    if (input_len < 64u || input_len % 64u != 0u) {
        return RVB_BAD_INPUT;
    }
    const uint32_t pair_count = input_len / 64u;
    if (pair_count > 32u) {
        return RVB_BAD_INPUT;
    }

    uint32_t folded = 0x58323535u;
    uint32_t crc = 0xffffffffu;
    uint8_t shared_secret[32];
    for (uint32_t pair = 0; pair < pair_count; ++pair) {
        const uint8_t *tuple = input + pair * 64u;
        crypto_x25519(shared_secret, tuple, tuple + 32u);
        crc = rvb_crc32_update(crc, shared_secret,
                               (uint32_t)sizeof(shared_secret));
        for (uint32_t word = 0; word < 8u; ++word) {
            folded = rvb_fold(folded, rvb_read_u32(shared_secret + word * 4u),
                              pair * 8u + word);
        }
    }
    crypto_wipe(shared_secret, sizeof(shared_secret));
    out[0] = rvb_crc32_finish(crc);
    out[1] = folded;
    return 0u;
}
