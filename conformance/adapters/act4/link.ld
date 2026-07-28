/* Place ACT4 code, data, and model hooks inside the EEI image area. */
/* SPDX-License-Identifier: Apache-2.0 */
OUTPUT_ARCH(riscv)
ENTRY(rvtest_entry_point)

PHDRS
{
  init PT_LOAD FLAGS(5);  /* R|X */
  test PT_LOAD FLAGS(5);  /* R|X */
  data PT_LOAD FLAGS(6);  /* R|W */
  model PT_LOAD FLAGS(5); /* R|X */
}

SECTIONS
{
  . = 0x00010000;
  .text.init : ALIGN(4) { KEEP(*(.text.init)) } :init
  .text.rvtest : ALIGN(4) { *(.text.rvtest) *(.text.rvtest.*) } :test

  . = ALIGN(0x1000);
  .data : ALIGN(16) {
    *(.data) *(.data.*) *(.rodata) *(.rodata.*)
    *(.sdata) *(.sdata.*) *(.sbss) *(.sbss.*) *(.bss) *(.bss.*)
    *(COMMON) *(.tohost)
  } :data

  /* Keeping model code after data preserves signature addresses across
     the Sail reference build and the final self-checking build. */
  . = ALIGN(0x1000);
  .text.rvmodel : ALIGN(4) {
    *(.text.rvmodel) *(.text.rvmodel.*) *(.text) *(.text.*)
  } :model

  . = ALIGN(0x1000);
  ASSERT(. <= 0x03000000, "ELF exceeds the EEI image area")
  /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.riscv.attributes) }
}
