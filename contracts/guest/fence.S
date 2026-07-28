.option norvc
.section .text
.globl _start

_start:
  .word 0x0ff0000f
  .word 0x0330000f
  .word 0x8330000f
  .word 0x5330000f
  .word 0xfff0810f
  .word 0x0000000f
  li a0, 0
  li a7, 0
exit_ecall:
  ecall
