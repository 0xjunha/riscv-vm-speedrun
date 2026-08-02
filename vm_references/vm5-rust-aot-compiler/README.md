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

## Optional profiler

The `profile` Cargo feature builds a diagnostic VM5 that writes one JSON object
to standard error after every `RUN`. The feature and all of its counters and
per-block metadata are absent from default builds.

```sh
cargo build --locked --release --features profile \
  --manifest-path vm_references/vm5-rust-aot-compiler/Cargo.toml
```

Each record has `kind: "vm5_profile"` and `schema_version: 2`. It reports:

- total, native, and interpreter-fallback retired instructions;
- native invocations, executed blocks, direct linked-edge hits, native exit
  reasons, and lookup versus short-budget fallbacks;
- fallback classes (loads, stores, JALR, M operations, system, other, and fetch
  traps) and a base-opcode breakdown;
- generated guest-register loads and stores, weighted by native block
  dispatches;
- native fallthrough, conditional-branch, and direct-jump dispatches; and
- LOAD-time compiled block, native guest instruction, raw code byte, mapped
  byte, and block control-flow counts.

Fallback class counts describe attempted interpreter instructions, so a
trapping instruction appears in its class but not in `retired.fallback`.
`lookup_fallbacks` means no published native entry existed at the current PC;
`budget_fallbacks` means an entry existed but would have crossed the exact
instruction limit. LOAD measurements are repeated unchanged on each RUN of the
same loaded image so every JSON line is self-contained.
