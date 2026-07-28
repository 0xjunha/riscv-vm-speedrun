.option norvc
.section .text
.globl _start

#if defined(CASE_EBREAK)
_start:
fault:
  ebreak

#elif defined(CASE_LOAD_MISALIGNED)
_start:
fault:
  lw t0, 1(zero)

#elif defined(CASE_LOAD_ACCESS)
_start:
fault:
  lw t0, 0(zero)

#elif defined(CASE_STORE_MISALIGNED)
_start:
fault:
  sw zero, 1(zero)

#elif defined(CASE_STORE_ACCESS)
_start:
fault:
  sw zero, 0(zero)

#elif defined(CASE_STORE_CODE)
_start:
  lui t0, 0x10
fault:
  sw zero, 0(t0)

#elif defined(CASE_STORE_INPUT)
_start:
fault:
  sb zero, 0(a0)

#elif defined(CASE_BRANCH_MISALIGNED)
_start:
fault:
  .word 0x00000163

#elif defined(CASE_BRANCH_NOT_TAKEN)
_start:
  .word 0x00001163
  li a0, 0
  li a7, 0
exit_ecall:
  ecall

#elif defined(CASE_JAL_MISALIGNED)
_start:
fault:
  .word 0x002002ef

#elif defined(CASE_JALR_MISALIGNED)
_start:
  lui t0, 0x10
  addi t0, t0, 2
fault:
  jalr t1, 0(t0)

#elif defined(CASE_FETCH_UNMAPPED)
_start:
  lui t0, 0x40
  jalr zero, 0(t0)

#elif defined(CASE_FETCH_NONEXEC)
_start:
  lui t0, 0x20
  jalr t1, 0(t0)

#elif defined(CASE_JALR_LOW_BIT)
_start:
  lui t0, 0x20
  addi t0, t0, 1
  jalr t1, 0(t0)

#else
#error "select one fault contract case"
#endif
