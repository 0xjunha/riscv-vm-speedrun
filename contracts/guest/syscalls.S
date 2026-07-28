.option norvc
.section .text
.globl _start

#if defined(CASE_WRITE_EXIT)
_start:
  li a7, 1
  ecall
  mv t0, a0
  li a0, 7
  li a7, 0
exit_ecall:
  ecall

#elif defined(CASE_APPEND)
_start:
  mv t0, a0
  li a1, 2
  li a7, 1
  ecall
  addi a0, t0, 2
  li a1, 2
  li a7, 1
  ecall
  li a0, 0
  li a7, 0
exit_ecall:
  ecall

#elif defined(CASE_ZERO_LENGTH)
_start:
  li a0, -1
  li a1, 0
  li a7, 1
  ecall
  li a0, 0
  li a7, 0
exit_ecall:
  ecall

#elif defined(CASE_INVALID_SYSCALL)
_start:
  li a7, 99
fault:
  ecall

#elif defined(CASE_INVALID_SOURCE)
_start:
  li a0, 0
  li a1, 4
  li a7, 1
fault:
  ecall

#elif defined(CASE_WRAPPING_SOURCE)
_start:
  li a0, -1
  li a1, 2
  li a7, 1
fault:
  ecall

#elif defined(CASE_OUTPUT_LIMIT)
_start:
  li a1, 4
  li a7, 1
fault:
  ecall

#elif defined(CASE_OUTPUT_BEFORE_TRAP)
_start:
  li a1, 1
  li a7, 1
  ecall
  li a7, 99
fault:
  ecall

#elif defined(CASE_OUTPUT_BEFORE_LIMIT)
_start:
  li a1, 1
  li a7, 1
  ecall
next_instruction:
  nop

#elif defined(CASE_FAILED_WRITE_PRESERVES_OUTPUT)
_start:
  li a1, 1
  li a7, 1
  ecall
  lui a0, 0x3400
  addi a0, a0, -2
  li a1, 4
fault:
  ecall

#else
#error "select one syscall contract case"
#endif
