#include "rvb_workload_common.h"

#include <stdint.h>

#define MATRIX_SIZE 20u
#define MATRIX_VALUES (MATRIX_SIZE * MATRIX_SIZE)
#define RECORD_VALUES (MATRIX_VALUES + MATRIX_SIZE)
#define RECORD_SIZE (RECORD_VALUES * 4u)

extern long int a[20][20];
extern long int b[20];
extern long int x[20];
int ludcmp(int maximum, int last_index);

uint32_t rvb_ud(const uint8_t *input, uint32_t input_len, uint32_t out[2]) {
    if (input_len == 0u || input_len % RECORD_SIZE != 0u ||
        input_len / RECORD_SIZE > 8u) {
        return RVB_BAD_INPUT;
    }

    uint32_t solutions = 0x55444c55u;
    uint32_t factors = 0x4445434fu;
    uint32_t observation = 0u;
    for (uint32_t offset = 0u; offset < input_len; offset += RECORD_SIZE) {
        for (uint32_t row = 0u; row < MATRIX_SIZE; ++row) {
            for (uint32_t column = 0u; column < MATRIX_SIZE; ++column) {
                a[row][column] = (long int)(int32_t)rvb_read_u32(
                    input + offset + (row * MATRIX_SIZE + column) * 4u);
            }
            b[row] = (long int)(int32_t)rvb_read_u32(
                input + offset + (MATRIX_VALUES + row) * 4u);
            x[row] = 0;
        }
        if (ludcmp(20, 19) != 0) {
            return RVB_INTERNAL_ERROR;
        }
        for (uint32_t row = 0u; row < MATRIX_SIZE; ++row) {
            solutions = rvb_fold(solutions, (uint32_t)x[row], observation++);
            for (uint32_t column = 0u; column < MATRIX_SIZE; ++column) {
                factors = rvb_fold(factors, (uint32_t)a[row][column],
                                   row * MATRIX_SIZE + column);
            }
        }
    }

    out[0] = solutions;
    out[1] = factors;
    return 0u;
}
