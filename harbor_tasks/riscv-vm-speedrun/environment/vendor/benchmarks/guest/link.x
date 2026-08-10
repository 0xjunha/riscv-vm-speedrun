/* RV32IM benchmark EEI image layout. All PT_LOAD boundaries are pages. */
OUTPUT_ARCH(riscv)
ENTRY(_start)

PHDRS
{
  text   PT_LOAD FLAGS(5); /* PF_R | PF_X */
  rodata PT_LOAD FLAGS(4); /* PF_R */
  data   PT_LOAD FLAGS(6); /* PF_R | PF_W */
}

SECTIONS
{
  . = 0x00010000;

  .text : ALIGN(4)
  {
    KEEP(*(.text._start))
    *(.text .text.*)
  } :text

  . = ALIGN(0x1000);
  .rodata : ALIGN(4)
  {
    *(.rodata .rodata.*)
    *(.srodata .srodata.*)
  } :rodata

  . = ALIGN(0x1000);
  .data : ALIGN(4)
  {
    __data_start = .;
    *(.data .data.*)
    *(.sdata .sdata.*)
    __data_end = .;
  } :data

  PROVIDE(__global_pointer$ = __data_start + 0x800);

  .bss (NOLOAD) : ALIGN(16)
  {
    __bss_start = .;
    *(.sbss .sbss.*)
    *(.bss .bss.*)
    *(COMMON)
    __bss_end = .;
  } :data

  __image_end = ALIGN(0x1000);
  ASSERT(__image_end <= 0x03000000, "guest image exceeds EEI image area")

  /DISCARD/ :
  {
    *(.eh_frame .eh_frame_hdr)
    *(.comment)
  }
}
