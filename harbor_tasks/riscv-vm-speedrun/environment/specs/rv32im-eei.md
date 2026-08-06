# RV32IM Execution Environment Interface

This document defines the guest-visible behavior of the RV32IM virtual machine.
It is independent of any implementation, host operating system, or host tooling.

Instruction semantics come from the pinned
[RV32I specification](riscv-isa-manual-v20260120/src/unpriv/rv32.adoc) and
[M-extension specification](riscv-isa-manual-v20260120/src/unpriv/m-st-ext.adoc).
The imported [Sail model](sail-riscv-0.12/model/) provides a formal reference.
This document selects the supported ISA features and defines the execution environment around them.
The host representation of runs and results is defined by [`rv32vm-interface.md`](rv32vm-interface.md).

## 1. ISA Profile

The machine has one hart, `XLEN = 32`, little-endian byte-addressed memory, and
fixed-width 32-bit instructions. It implements RV32I 2.1 and M 2.0:

```text
LUI AUIPC
JAL JALR
BEQ BNE BLT BGE BLTU BGEU
LB LH LW LBU LHU
SB SH SW
ADDI SLTI SLTIU XORI ORI ANDI
SLLI SRLI SRAI
ADD SUB SLL SLT SLTU XOR SRL SRA OR AND
FENCE ECALL EBREAK
MUL MULH MULHSU MULHU
DIV DIVU REM REMU
```

All `MISC-MEM` encodings with `funct3 = 000`, including `FENCE`, `FENCE.TSO`,
hints, and reserved-`fm` fallback behavior, are accepted as no-ops. The
environment has no observable device or inter-hart ordering.

`FENCE.I`, CSR instructions, and every instruction from C, A, F, D, Q, B, V,
the privileged architecture, or any other extension are unsupported.
Unsupported and reserved instructions raise `IllegalInstruction`. There are
no interrupts, privilege levels, address translation, memory-mapped devices,
or self-modifying executable code.

Only the standard encodings `0x0000_0073` for `ECALL` and `0x0010_0073` for
`EBREAK` are accepted from the `SYSTEM` opcode.

The standard RV32IM rules apply, including:

- arithmetic wraps modulo `2^32`;
- signed operations use two's-complement values;
- register shift counts use their low five bits;
- `x0` always reads as zero and discards writes;
- division by zero and signed division overflow follow the M extension and do
  not trap; and
- `JALR` clears bit zero of its target.

## 2. Machine State

The guest-visible state consists of:

- 32 general-purpose 32-bit registers, `x0` through `x31`;
- a 32-bit program counter;
- memory described below;
- an append-only byte output stream; and
- a 64-bit retired-instruction counter.

There are no guest-visible clocks, cycle counters, CSRs, random sources, host
paths, environment variables, or other host state.

## 3. Address Space

The address space is 64 MiB, with 4 KiB pages:

| Range | Use | Permission |
|---|---|---|
| `0x0000_0000..0x0001_0000` | null guard | unmapped |
| `0x0001_0000..0x0300_0000` | ELF image and guest heap | defined by ELF segments |
| `0x0300_0000..0x0340_0000` | input | read-only |
| `0x0340_0000..0x0380_0000` | guard | unmapped |
| `0x0380_0000..0x0400_0000` | downward-growing stack | read/write |

`0x0400_0000` is one byte beyond the address space and is the initial stack
pointer.

An image page is unmapped, read-only, read/write, or read/execute. Writable
and executable permissions may not coexist. Instruction fetch requires
execute permission, loads require read permission, and stores require write
permission. After alignment validation, a store to an executable page raises
`StoreAccessFault`.

Every byte of an access must be mapped with the required permission. Guest
address calculations wrap to 32 bits, but validation of the resulting byte
range uses non-wrapping arithmetic. An access whose byte range wraps around
address zero faults.

## 4. ELF Image

The machine accepts a static executable with all of these properties:

- ELF32, little-endian, current ELF version, `ET_EXEC`, and `EM_RISCV`;
- the standard ELF32 header and program-header entry sizes, and the standard
  40-byte section-header entry size when section headers are present;
- `e_flags = 0`;
- a nonempty, non-extended program-header table;
- no interpreter, dynamic linking, runtime relocation, TLS, or executable
  `PT_GNU_STACK`;
- no extended section numbering; if section headers are present, their table
  and string-table index are valid, no allocated `SHT_DYNAMIC`, `SHT_REL`, or
  `SHT_RELA` section is present, and no section has `SHF_TLS`;
- at least one nonempty `PT_LOAD` segment; and
- a 4-byte-aligned entry point in an executable loaded page.

Every `PT_LOAD` is checked for:

- `p_filesz <= p_memsz`, and the file range is valid;
- `p_align` is zero, one, or a power of two, and `p_vaddr` and `p_offset` have
  the required congruence when `p_align > 1`.

A `PT_LOAD` with `p_memsz = 0` is then ignored. It maps no page, may otherwise
contain arbitrary address and permission fields, and does not satisfy the
requirement for a nonempty load segment.

Each nonempty `PT_LOAD` must additionally satisfy:

- the virtual range lies entirely in
  `0x0001_0000..0x0300_0000` and does not overlap another load segment;
- `p_flags` is exactly read-only, read/write, or read/execute;
- the union of segment permissions never makes a page writable and
  executable; and
- for an executable segment, `p_vaddr`, `p_filesz`, and `p_memsz` are
  multiples of four.

Loading copies `p_filesz` bytes and zero-fills the remaining
`p_memsz - p_filesz` bytes. Bytes exposed by a mapped page but not supplied by
a segment are also zero. Unsupported instruction words in executable data do
not make the ELF invalid; they raise `IllegalInstruction` only if fetched.

## 5. Run Initialization

Each run begins from the loaded image's pristine state:

| State | Initial value |
|---|---|
| all registers except `x2`, `x10`, and `x11` | `0` |
| `pc` | ELF entry point |
| `x2` (`sp`) | `0x0400_0000` |
| `x10` (`a0`) | `0x0300_0000` |
| `x11` (`a1`) | input length |
| output | empty |
| retired instructions | `0` |

The input bytes begin at `0x0300_0000`; the rest of the input area is
zero-filled and read-only. The stack, ELF BSS, and all other guest-writable
memory are restored to their pristine post-load contents before every run.
Registers not listed specially above, including `ra` and `gp`, remain zero.
Guest startup code initializes `gp` if needed.

An implementation may retain decoded or generated code for the same immutable
image, but registers, memory, output, traps, allocation state, and resource
counters must never leak between runs.

## 6. Execution and Faults

Instruction addresses must be 4-byte aligned. Byte accesses have no alignment
requirement; halfword and word accesses require 2-byte and 4-byte alignment,
respectively. Misaligned data accesses are not emulated.

A taken branch, `JAL`, or `JALR` with a misaligned target raises
`InstructionAddressMisaligned` at the control-transfer instruction. An aligned
target outside executable memory is committed normally; the following fetch
raises `InstructionAccessFault`.

For a data access, alignment is checked before range and permission. Each
instruction follows this order:

1. check the retired-instruction limit described in section 9;
2. check fetch alignment;
3. check fetch mapping and execute permission;
4. fetch and decode the 32-bit instruction;
5. check instruction legality;
6. compute operands and effective addresses;
7. check data alignment, then the complete data range and permissions;
8. validate any syscall and its limits; and
9. commit all effects atomically.

Except for a successful exit `ECALL`, a successful instruction that does not
otherwise replace `pc` advances it by four. A trapping instruction changes no
destination register, memory byte, output byte, or `pc`, and does not retire.
Stores and output operations must be fully validated before modifying any
state. `x0` remains zero in every case.

## 7. System Calls

`ECALL` reads its operation number from `a7` (`x17`). No other system calls
exist.

### 7.1 Exit

```text
a7 = 0
a0 = unsigned 32-bit exit code
```

The run terminates with status `exit`, preserving existing output. The `ECALL`
retires, but `pc` is not advanced.

### 7.2 Write Output

```text
a7 = 1
a0 = guest source address
a1 = byte length
```

For length zero, the operation succeeds without validating the address.
Otherwise, the complete source range must be readable before the output limit
is checked. The operation appends all bytes atomically, writes the requested
length to `a0`, advances `pc` by four, and retires.

An invalid source range raises `LoadAccessFault`. A result larger than the
configured output limit raises `OutputLimitExceeded`. An unknown operation
number raises `InvalidSyscall`.

## 8. Traps

Traps are terminal; the guest cannot install a handler. A trap result contains
the cause, the `pc` of the instruction or fetch that caused it, and a 32-bit
value:

| Cause | Value |
|---|---|
| `InstructionAddressMisaligned` | attempted target |
| `InstructionAccessFault` | faulting fetch address |
| `IllegalInstruction` | raw instruction word |
| `Breakpoint` | `0` |
| `LoadAddressMisaligned` | effective address |
| `LoadAccessFault` | effective address |
| `StoreAddressMisaligned` | effective address |
| `StoreAccessFault` | effective address |
| `InvalidSyscall` | syscall number |
| `OutputLimitExceeded` | requested resulting output length modulo `2^32` |

`EBREAK` raises `Breakpoint`. Failed fetches, instructions, and system calls do
not retire.

## 9. Instruction and Output Limits

Before every instruction fetch, execution compares the retired-instruction
counter with the configured instruction limit. If the counter is greater than
or equal to the limit, the run ends with resource failure
`InstructionLimit`.

Each successfully committed instruction increments the counter once,
including successful exit and write-output `ECALL`s. A trap does not increment
it. A limit of zero therefore stops before the initial fetch, and a limit of
`N` permits at most `N` committed instructions. The limit check occurs before
fetch, so exhaustion takes priority over a fault that the next fetch would
otherwise cause.

The run bounds are:

| Resource | Default | Maximum |
|---|---:|---:|
| input bytes | — | `4,194,304` |
| output bytes | `1,048,576` | `1,048,576` |
| retired instructions | `100,000,000` | `1,000,000,000` |

The caller may choose smaller instruction and output limits. Host time and
memory limits are outside the guest execution environment.

## 10. Determinism

For fixed ELF bytes, input bytes, and run limits, the final registers, memory,
output, result, and retirement count must be identical across implementations
and repeated runs. Timing, CPU model, process state, address-space layout,
filesystem paths, and permitted implementation caches must not affect
guest-visible behavior.
