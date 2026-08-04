#ifndef RVB_C_WORKLOADS_H
#define RVB_C_WORKLOADS_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/*
 * Every adapter returns zero on success and writes the primary and auxiliary
 * result words to out[0] and out[1]. On failure it returns a stable nonzero
 * status and leaves the output words unspecified.
 */
uint32_t rvb_littlefs(const uint8_t *input, uint32_t input_len,
                      uint32_t out[2]);
uint32_t rvb_x25519(const uint8_t *input, uint32_t input_len,
                    uint32_t out[2]);

#ifdef __cplusplus
}
#endif

#endif
