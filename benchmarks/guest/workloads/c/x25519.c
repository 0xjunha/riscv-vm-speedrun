/* Project-owned adapter around the pinned upstream Monocypher workload. */

#include "rvb_workload_common.h"
#include "rvb_c_workloads.h"

#include "monocypher.h"

#include <stdint.h>

uint32_t rvb_x25519(const uint8_t *input, uint32_t input_len,
                    uint32_t out[2]) {
    if (input_len < 72u) {
        return RVB_BAD_INPUT;
    }
    const uint32_t repetitions = rvb_read_u32(input);
    const uint32_t pair_count = rvb_read_u32(input + 4u);
    if (repetitions == 0u || repetitions > 32u || pair_count == 0u ||
        pair_count > 32u || input_len != 8u + pair_count * 64u) {
        return RVB_BAD_INPUT;
    }

    uint32_t aggregate = 0x58323535u;
    uint32_t final_crc = 0u;
    uint8_t shared_secret[32];
    for (uint32_t pass = 0; pass < repetitions; ++pass) {
        uint32_t crc = 0xffffffffu;
        for (uint32_t pair = 0; pair < pair_count; ++pair) {
            const uint8_t *tuple = input + 8u + pair * 64u;
            crypto_x25519(shared_secret, tuple, tuple + 32u);
            crc = rvb_crc32_update(crc, shared_secret,
                                   (uint32_t)sizeof(shared_secret));
        }
        final_crc = rvb_crc32_finish(crc);
        aggregate = rvb_fold(aggregate, final_crc, pass);
    }
    crypto_wipe(shared_secret, sizeof(shared_secret));
    out[0] = aggregate;
    out[1] = final_crc;
    return 0u;
}
