// EEI adapter for the BSD-licensed riscv-tests assembly sources.
// SPDX-License-Identifier: BSD-3-Clause

#ifndef RV32IM_CONFORMANCE_RISCV_TEST_H
#define RV32IM_CONFORMANCE_RISCV_TEST_H

#define RVTEST_RV32U
#define RVTEST_RV64U
#define TESTNUM gp

#define RVTEST_CODE_BEGIN       \
  .section .text.init,"ax";     \
  .balign 4;                    \
  .globl _start;                \
_start:                         \
  .option push;                 \
  .option norvc;                \
  .option norelax

#define RVTEST_CODE_END .option pop

// EEI syscall 0 exits; a0 is the process exit code.
#define RVTEST_PASS             \
  li a0, 0;                     \
  li a7, 0;                     \
  ecall

#define RVTEST_FAIL             \
  bnez TESTNUM, 1f;             \
  li TESTNUM, 1;                \
1:                              \
  mv a0, TESTNUM;               \
  li a7, 0;                     \
  ecall

#define RVTEST_DATA_BEGIN       \
  .balign 16;                   \
  .globl begin_signature;       \
begin_signature:

#define RVTEST_DATA_END         \
  .balign 16;                   \
  .globl end_signature;         \
end_signature:

#endif
