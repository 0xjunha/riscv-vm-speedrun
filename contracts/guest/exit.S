.option norvc
.section .text
.globl _start

_start:
  li a0, EXIT_CODE
  li a7, 0
exit_ecall:
  ecall
