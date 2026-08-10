#include "rvb_workload_common.h"
#include "slre.h"

#include <stdint.h>
#include <string.h>

uint32_t rvb_slre(const uint8_t *input, uint32_t input_len, uint32_t out[2]) {
    uint32_t offset = 0u;
    uint32_t matches = 0x534c5245u;
    uint32_t captures = 0x52454758u;
    uint32_t record = 0u;

    while (offset < input_len) {
        if (input_len - offset < 3u || record >= 128u) {
            return RVB_BAD_INPUT;
        }
        const uint32_t pattern_len = input[offset];
        const uint32_t text_len = (uint32_t)input[offset + 1u] |
                                  ((uint32_t)input[offset + 2u] << 8);
        offset += 3u;
        if (pattern_len == 0u || pattern_len > 63u || text_len == 0u ||
            text_len > 512u || pattern_len + text_len > input_len - offset) {
            return RVB_BAD_INPUT;
        }

        char pattern[64];
        for (uint32_t i = 0u; i < pattern_len; ++i) {
            if (input[offset + i] == 0u) {
                return RVB_BAD_INPUT;
            }
            pattern[i] = (char)input[offset + i];
        }
        pattern[pattern_len] = '\0';
        const char *text = (const char *)(input + offset + pattern_len);
        struct slre_cap capture = {NULL, 0};
        const int matched =
            slre_match(pattern, text, (int)text_len, &capture, 1);
        matches = rvb_fold(matches, (uint32_t)matched, record);

        uint32_t observation = UINT32_MAX;
        if (matched >= 0 && capture.ptr != NULL && capture.ptr >= text &&
            capture.ptr <= text + text_len && capture.len >= 0 &&
            (uint32_t)capture.len <= text_len - (uint32_t)(capture.ptr - text)) {
            observation = ((uint32_t)(capture.ptr - text) << 16) ^
                          (uint32_t)capture.len;
        }
        captures = rvb_fold(captures, observation, record);
        offset += pattern_len + text_len;
        ++record;
    }
    if (record == 0u) {
        return RVB_BAD_INPUT;
    }

    out[0] = matches;
    out[1] = captures;
    return 0u;
}
