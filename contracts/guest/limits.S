.option norvc
.section .text
.globl _start

#if defined(CASE_LOOP)
_start:
  jal zero, _start

#elif defined(CASE_BRANCH_UNMAPPED)
_start:
  lui t0, 0x40
transfer:
  jalr zero, 0(t0)

#elif defined(CASE_STRAIGHT_LINE)
_start:
  addi t0, t0, 1
  addi t0, t0, 1
  addi t0, t0, 1
  addi t0, t0, 1
  addi t0, t0, 1
  addi t0, t0, 1
  addi t0, t0, 1
  addi t0, t0, 1
  addi t0, t0, 1
  addi t0, t0, 1
  li a0, 0
  li a7, 0
  ecall

#else
#error "select one limit contract case"
#endif
