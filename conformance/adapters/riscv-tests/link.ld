/* Place a riscv-tests image inside the project EEI image area. */
/* SPDX-License-Identifier: BSD-3-Clause */
OUTPUT_ARCH(riscv)
ENTRY(_start)

PHDRS
{
  text PT_LOAD FLAGS(5); /* R|X */
  data PT_LOAD FLAGS(6); /* R|W */
}

SECTIONS
{
  . = 0x00010000;
  .text : ALIGN(4) { *(.text.init) *(.text) *(.text.*) } :text
  . = ALIGN(0x1000);
  .data : ALIGN(16) {
    *(.data) *(.data.*) *(.rodata) *(.rodata.*)
    *(.sdata) *(.sdata.*) *(.sbss) *(.sbss.*) *(.bss) *(.bss.*)
    *(COMMON)
    LONG(0)
  } :data
  . = ALIGN(0x1000);
  ASSERT(. <= 0x03000000, "ELF exceeds the EEI image area")
  /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.riscv.attributes) }
}
