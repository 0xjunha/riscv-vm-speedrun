#include "rvb_workload_common.h"

#include <stdint.h>

void embench_statemate_warm_caches(int heat);
void interface(void);
void FH_DU(void);

extern unsigned long time;
extern int FH_TUERMODUL__POSITION;
extern int FH_TUERMODUL__I_EIN;
extern char FH_TUERMODUL__SFHZ_ZENTRAL;
extern char FH_TUERMODUL__SFHZ_MEC;
extern char FH_TUERMODUL__SFHA_ZENTRAL;
extern char FH_TUERMODUL__SFHA_MEC;
extern char FH_TUERMODUL__KL_50;
extern char FH_TUERMODUL__EKS_LEISTE_AKTIV;
extern char FH_TUERMODUL__COM_OPEN;
extern char FH_TUERMODUL__COM_CLOSE;
extern char KINDERSICHERUNG_CTRL_KINDERSICHERUNG_CTRL_next_state;
extern char B_FH_TUERMODUL_CTRL_next_state;
extern char A_FH_TUERMODUL_CTRL_next_state;
extern char EINKLEMMSCHUTZ_CTRL_EINKLEMMSCHUTZ_CTRL_next_state;
extern char BLOCK_ERKENNUNG_CTRL_BLOCK_ERKENNUNG_CTRL_next_state;

static void reset_inputs(void) {
    time = 0u;
    FH_TUERMODUL__POSITION = 0;
    FH_TUERMODUL__I_EIN = 0;
    FH_TUERMODUL__SFHZ_ZENTRAL = 0;
    FH_TUERMODUL__SFHZ_MEC = 0;
    FH_TUERMODUL__SFHA_ZENTRAL = 0;
    FH_TUERMODUL__SFHA_MEC = 0;
    FH_TUERMODUL__KL_50 = 0;
    FH_TUERMODUL__EKS_LEISTE_AKTIV = 0;
}

uint32_t rvb_statemate(const uint8_t *input, uint32_t input_len,
                       uint32_t out[2]) {
    if (input_len < 16u || input_len % 4u != 0u || input_len / 4u > 1024u) {
        return RVB_BAD_INPUT;
    }

    reset_inputs();
    embench_statemate_warm_caches(1);
    uint32_t outputs = 0x53544154u;
    uint32_t states = 0x4d415445u;

    for (uint32_t offset = 0u, index = 0u; offset < input_len;
         offset += 4u, ++index) {
        const uint8_t flags = input[offset];
        FH_TUERMODUL__SFHZ_ZENTRAL = (char)(flags & 1u);
        FH_TUERMODUL__SFHA_ZENTRAL = (char)((flags >> 1) & 1u);
        FH_TUERMODUL__SFHZ_MEC = (char)((flags >> 2) & 1u);
        FH_TUERMODUL__SFHA_MEC = (char)((flags >> 3) & 1u);
        FH_TUERMODUL__KL_50 = (char)((flags >> 4) & 1u);
        FH_TUERMODUL__EKS_LEISTE_AKTIV = (char)((flags >> 5) & 1u);
        FH_TUERMODUL__POSITION = (int)input[offset + 1u] * 2;
        FH_TUERMODUL__I_EIN =
            (int)(int16_t)((uint16_t)input[offset + 2u] |
                           ((uint16_t)input[offset + 3u] << 8));
        ++time;
        interface();
        FH_DU();

        const uint32_t output_bits =
            (uint32_t)(uint8_t)FH_TUERMODUL__COM_OPEN |
            ((uint32_t)(uint8_t)FH_TUERMODUL__COM_CLOSE << 8) |
            ((uint32_t)(uint8_t)B_FH_TUERMODUL_CTRL_next_state << 16) |
            ((uint32_t)(uint8_t)A_FH_TUERMODUL_CTRL_next_state << 24);
        const uint32_t state_bits =
            (uint32_t)(uint8_t)KINDERSICHERUNG_CTRL_KINDERSICHERUNG_CTRL_next_state |
            ((uint32_t)(uint8_t)EINKLEMMSCHUTZ_CTRL_EINKLEMMSCHUTZ_CTRL_next_state
             << 8) |
            ((uint32_t)(uint8_t)BLOCK_ERKENNUNG_CTRL_BLOCK_ERKENNUNG_CTRL_next_state
             << 16) |
            ((uint32_t)FH_TUERMODUL__POSITION << 20);
        outputs = rvb_fold(outputs, output_bits, index);
        states = rvb_fold(states, state_bits ^ (uint32_t)FH_TUERMODUL__I_EIN,
                          index);
    }

    out[0] = outputs;
    out[1] = states;
    return 0u;
}
