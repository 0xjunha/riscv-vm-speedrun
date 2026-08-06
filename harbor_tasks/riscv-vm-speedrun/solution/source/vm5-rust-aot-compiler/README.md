# RV32IM Rust AOT Compiler

VM5 is an eager ahead-of-time (AOT) compiler for RV32IM programs. It compiles
an ELF into reusable x86-64 code during `LOAD`; it never compiles during
`RUN`.

VM5 runs only on x86-64 Linux.

## How it works

- **`LOAD`:** discover executable code, compile native regions, link them, and
  publish one immutable native image.
- **`RUN`:** reuse that image without recompiling it.
- **`UNLOAD`:** discard the image.
- **Fallback:** use the shared interpreter for system instructions,
  unsupported code, traps, and precise slow paths.

A **native region** is a sequence of guest instructions compiled as one unit.
VM5 natively handles common RV32I integer and control-flow instructions, RV32M
arithmetic, and checked integer loads and stores.

## Main performance choices

### 1. Compile once during `LOAD`

- Compilation finishes before the first `RUN`.
- Every later `RUN` reuses the same native image.
- There is no profiling threshold or background JIT compiler.

**Benefit:** compilation work never interrupts steady-state execution.

### 2. Link native regions directly

- VM5 follows control flow from the ELF entry point, then performs a bounded
  scan for code reachable only through indirect jumps.
- Known branches and jumps are linked directly to native destinations.
- Adjacent regions fall through without an extra host jump.
- JALR finds compiled targets through an immutable two-level table.

**Benefit:** most control flow stays in machine code instead of returning to
the Rust dispatcher between regions.

### 3. Cache frequently used guest registers

- VM5 may keep six frequently used RV32 registers in host registers for the
  whole image.
- A region may cache one additional profitable register.
- Selection uses generic register-use and loop information. Caching is enabled
  only when its estimated savings exceed its entry and exit cost.
- Arithmetic uses cached registers or the guest-register array directly.

**Benefit:** loops perform fewer guest-register loads, stores, and temporary
moves.

### 4. Access checked guest memory directly

- VM5 uses a contiguous host view of the 32-bit guest address space.
- Native loads and stores check guest-page permissions; wider accesses also
  check alignment.
- Several memory operations can remain in one native region.
- On failure, completed instructions remain committed and the interpreter
  retries exactly the failing instruction.

**Benefit:** valid accesses use a short native path without weakening memory
permissions, traps, or retirement accounting.

### 5. Move rare work out of hot code

- Budget failures, memory retries, missing targets, and exits use shared cold
  stubs.
- Profitable register caches share entry and exit setup; uncached images keep
  setup inline to avoid extra indirection.
- Direct links enter hot bodies. Indirect entries retain valid hardware
  control-flow landing pads, which adjacent edges can share.

**Benefit:** frequently executed code is smaller and contains less duplicated
setup and exit logic.

## Correctness and safety

- A region reserves its full instruction budget before changing guest state.
- Memory and JALR slow paths preserve exact traps, register state, and retired
  instruction counts.
- Unsupported or unavailable code continues in the shared interpreter.
- All reusable code is produced during `LOAD`; `RUN` never modifies it.
- Generated memory changes from writable to read-execute when published. It is
  never writable and executable at the same time.
- Dispatch tables are separate and never executable.

## Resource limits

- **64 guest instructions** per native region.
- **262,144 file-backed instructions** scanned.
- **8,192 native regions** retained.
- One **32 MiB native-code arena** per image.
- At most **32.1875 MiB** of indirect-dispatch data.
- Space for entries, exits, retries, and missing targets is reserved before a
  region is accepted. Code that does not fit remains interpreted.

## Build and verification

```sh
make vm5
make vm5-conformance
make vm5-contract
make vm5-benchmark-smoke
make vm5-x86-check
```

## Optional profiler

The `profile` feature builds a diagnostic VM5. It is absent from normal builds.

```sh
cargo build --locked --release --features profile \
  --manifest-path vm_references/vm5-rust-aot-compiler/Cargo.toml
```

After each `RUN`, the diagnostic build writes one JSON record to standard
error. Records use `kind: "vm5_profile"` and `schema_version: 6` and summarize:

- native coverage and fallback reasons;
- direct and indirect control flow;
- register-cache and memory traffic; and
- generated-code, mapping, cache, and dispatch-table sizes.

A trapping fallback is classified but not retired. A lookup fallback means no
native entry existed; a budget fallback means running it would cross the exact
instruction limit.
