// ACT4 model hooks for the project's RV32IM EEI.
// SPDX-License-Identifier: Apache-2.0

#ifndef RV32IM_CONFORMANCE_RVMODEL_MACROS_H
#define RV32IM_CONFORMANCE_RVMODEL_MACROS_H

// Keep reference and final binaries' test-visible addresses identical.
#define RVMODEL_DATA_SECTION                                      \
  .pushsection .tohost,"aw",@progbits;                            \
  .balign 8; .global tohost; tohost: .dword 0;                    \
  .balign 8; .global fromhost; fromhost: .dword 0;                \
  .popsection

// The EEI has no privilege modes or CSR startup.
#define RVMODEL_BOOT
#define RVMODEL_BOOT_TO_MMODE

// EEI syscall 0 exits; a0 is the process exit code.
#define RVMODEL_HALT_PASS li a0, 0; li a7, 0; ecall
#define RVMODEL_HALT_FAIL li a0, 1; li a7, 0; ecall

#define RVMODEL_IO_INIT(_R1, _R2, _R3)
#define RVMODEL_IO_WRITE_STR(_R1, _R2, _R3, _STR_PTR)

// ACT4 requires these hooks even when interrupt tests are not selected.
#define RVMODEL_INTERRUPT_LATENCY 1
#define RVMODEL_TIMER_INT_SOON_DELAY 1
#define RVMODEL_SET_MEXT_INT(_R1, _R2)
#define RVMODEL_CLR_MEXT_INT(_R1, _R2)
#define RVMODEL_SET_MSW_INT(_R1, _R2)
#define RVMODEL_CLR_MSW_INT(_R1, _R2)
#define RVMODEL_SET_SEXT_INT(_R1, _R2)
#define RVMODEL_CLR_SEXT_INT(_R1, _R2)
#define RVMODEL_SET_SSW_INT(_R1, _R2)
#define RVMODEL_CLR_SSW_INT(_R1, _R2)

#endif
