.option norvc
.section .text
.globl _start

#if defined(CASE_PRISTINE)
_start:
  lui t0, 0x20
  lw t1, 0(t0)
  addi t1, t1, 1
  sw t1, 0(t0)
  addi s0, s0, 1
  lw t2, -4(sp)
  addi t2, t2, 1
  sw t2, -4(sp)
  addi a0, sp, -16
  sw t1, 0(a0)
  sw s0, 4(a0)
  sw t2, 8(a0)
  li a1, 12
  li a7, 1
  ecall
  li a0, 0
  li a7, 0
exit_ecall:
  ecall

#elif defined(CASE_INPUT_TAIL)
_start:
  li a1, 4
  li a7, 1
  ecall
  li a0, 0
  li a7, 0
exit_ecall:
  ecall

#elif defined(CASE_UNFETCHED_ILLEGAL)
_start:
  jal zero, after_illegal
  .word 0xffffffff
after_illegal:
  li a0, 0
  li a7, 0
exit_ecall:
  ecall

#else
#error "select one state contract case"
#endif
