# RV32IM Rust AOT Compiler

An RV32IM interpreter that compiles supported instruction blocks into native
x86-64 code when an ELF is loaded. This follows the VM interface: `LOAD`
supplies the program, and later `RUN` requests reuse its compiled blocks until
the image is replaced or unloaded.

Memory operations, division, indirect jumps, syscalls, traps, short instruction
budgets, and unsupported instructions use the shared interpreter.

Translation scans at most 262,144 file-backed instructions (1 MiB of RV32
code), retains at most 8,192 native blocks and their lookup metadata, and stores
their page-rounded x86-64 code in one 32 MiB arena. These limits bound `LOAD`
time and memory; code beyond them runs in the interpreter. The arena is written
before it is changed to read-execute, so it is never writable and executable at
the same time.

The VM executable runs only on x86-64 Linux.

```sh
make vm5
make vm5-conformance
make vm5-contract
make vm5-benchmark-smoke
make vm5-x86-check
```
