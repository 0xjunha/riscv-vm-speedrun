# Contract Guest Programs

These RV32IM programs test guest-visible behavior from
[`rv32im-eei.md`](../../interface_specs/rv32im-eei.md). They have no runtime
library or operating-system dependency.

The `.S` suffix means assembly with C preprocessing. `cases.json` selects a
program branch or value using build definitions such as `CASE_EBREAK` or
`EXIT_CODE=7`. Each resulting program starts at `_start` and uses only
fixed-width 32-bit instructions.

## System Calls

Guest programs use the project-specific
[`ECALL` interface](../../interface_specs/rv32im-eei.md#7-system-calls).

Before the first instruction of every VM run, the
[run-initialization rules](../../interface_specs/rv32im-eei.md#5-run-initialization)
copy the supplied input to guest address `0x0300_0000`, set `a0` to that
address, and set `a1` to the input length. It allows a guest to write its
complete input by selecting the write operation and executing `ECALL`.

## Files

- `exit.S` exits with a selected code.
- `faults.S` triggers specific memory and control-flow faults.
- `fence.S` exercises accepted `MISC-MEM` encodings.
- `limits.S` tests instruction limits and their priority over fetch faults.
- `raw_instruction.S` executes one exact instruction word.
- `state.S` tests pristine state, input clearing, and unfetched code.
- `syscalls.S` tests exit, output, and syscall failure behavior.
- `link.ld` places executable code at the EEI image base and defines `_start`
  as the entry point.

## Build Path

For each executable case, `contracts/build.py`:

1. preprocesses and assembles the selected `.S` program;
2. links it with `link.ld`;
3. extracts its code bytes and symbol addresses; and
4. asks `contracts/elf_builder.py` to create the final controlled ELF.

The files in this directory are authored inputs. Generated ELFs belong under
`contracts/artifacts/` and should not be edited manually.
