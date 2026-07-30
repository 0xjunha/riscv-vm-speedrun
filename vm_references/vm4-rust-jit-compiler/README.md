# RV32IM Rust JIT Compiler

A tiered RV32IM interpreter that compiles frequently executed arithmetic and
control-flow instruction sequences into native x86-64 code.

Memory operations, syscalls, traps, short instruction budgets, and unsupported
instructions use the shared precise interpreter.

The engine starts compiling a cached block on its third complete execution.
The cache is capped at 8,192 blocks, 262,144 decoded instructions, and 16 MiB
of native mappings. It belongs to one loaded ELF and is released by `UNLOAD`.

Executable memory is written first and then changed to read-execute; it is
never writable and executable at the same time.

The VM executable runs only on x86-64 Linux.

```sh
make vm4
make vm4-conformance
make vm4-contract
make vm4-benchmark-smoke
make vm4-x86-check
```
