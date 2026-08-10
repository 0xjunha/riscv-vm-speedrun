# RV32IM Rust JIT Compiler

VM4 is a demand-driven just-in-time (JIT) compiler for RV32IM programs. It
starts by interpreting code, compiles hot blocks and paths during `RUN`, and
reuses the resulting x86-64 code for the loaded ELF.

VM4 runs only on x86-64 Linux.

## How it works

- **`LOAD`:** create an empty, image-scoped block and native-code cache.
- **`RUN`:** interpret cold blocks, compile hot blocks, then form larger native
  regions and loops from observed control flow.
- **Later `RUN`s:** reuse the decoded and compiled code for the same ELF.
- **`UNLOAD`:** discard the cache and all native mappings.
- **Fallback:** use the shared interpreter for system instructions, unsupported
  code, precise side exits, and short instruction-budget tails.

A **basic block** contains up to 64 guest instructions and never crosses a
4 KiB page boundary. VM4 natively handles common RV32I integer and control-flow
instructions, RV32M arithmetic, and checked integer loads and stores.

## Main performance choices

### 1. Compile only hot blocks

- A cached block starts compilation after its third complete interpreted
  execution.
- Cold, uncached, or untranslatable code stays in the interpreter.
- Compiled blocks remain available for later dispatches and `RUN`s.

**Benefit:** VM4 spends compilation time only on code that is actually reused.

### 2. Combine dominant paths into native regions

- VM4 starts a region when a native head edge has at least 8 observations and
  one target accounts for at least 7/8 of them.
- It follows dominant native edges for up to 8 blocks and 256 instructions.
- It compiles the longest valid prefix containing at least 2 blocks.
- A branch that leaves the predicted path exits at an exact block boundary.
- Profiles stop changing after a region is selected or after 64 observations
  fail to produce a dominant edge.

**Benefit:** common paths cross fewer Rust dispatch boundaries and share one
register and memory-check plan.

### 3. Turn stable cycles into counted native loops

- If the first head-closing prefix is a valid unique cycle, including a
  self-loop, VM4 emits one native cycle with a host backedge.
- Loops retain a tighter limit of 4 blocks and 128 instructions.
- One native call executes as many complete cycles as the remaining instruction
  budget permits; a shorter tail returns to normal dispatch.
- If loop validation fails, VM4 keeps the finite-region fallback.

**Benefit:** hot loops stay in machine code across guest iterations without
losing exact instruction-budget accounting.

### 4. Chain stable native successors

- Once an edge profile is frozen, VM4 can continue directly into its exact
  compiled successor.
- A chain may follow up to 32 basic or region successor hops before returning
  to the main dispatcher.
- Profiling edges, missing targets, side exits, and insufficient budgets stop
  the chain safely.

**Benefit:** already-compiled paths avoid repeated top-level cache lookup and
dispatch work.

### 5. Reduce work inside generated code

- Frequently reused guest registers stay in host registers: up to 3 in bounded
  entries and up to 6 in counted loops.
- Arithmetic reads cached or canonical guest operands directly.
- Complementary shifts may become one rotate, with dead intermediate shifts
  removed.
- Checked loads and stores access flat guest memory directly and reuse
  dominating alignment and permission checks when it is safe.

**Benefit:** native paths perform fewer register-file accesses, host
instructions, and repeated memory guards.

### 6. Pack native entries into executable cohorts

- Compiled blocks and regions are staged together in near-page-sized cohorts.
- Basic and region entries share the same exact native-mapping budget.
- Published code is immutable and reused until `UNLOAD`.

**Benefit:** VM4 reduces mapping overhead and wasted executable memory.

## Correctness and safety

- Native exits report the exact committed instruction prefix.
- A failing memory operation remains uncommitted and is retried precisely by
  the interpreter.
- Counted loops execute only complete cycles within the instruction budget.
- System instructions and unsupported operations remain interpreted.
- Executable memory changes from writable to read-execute when published. It is
  never writable and executable at the same time.

## Resource limits

- **64 guest instructions** per basic block.
- **8 blocks / 256 instructions** per finite region.
- **4 blocks / 128 instructions** per counted loop.
- **8,192 cached blocks** and **262,144 decoded instructions** per ELF.
- **16 MiB** total native mappings per ELF.
- Code beyond these limits remains interpreted.

## Build and verification

```sh
make vm4
make vm4-conformance
make vm4-contract
make vm4-benchmark-smoke
make vm4-x86-check
```

## Optional profiler

The `profile` feature builds a diagnostic VM4. Its state and recording calls
are absent from normal builds.

```sh
cargo build --locked --release --features profile \
  --manifest-path vm_references/vm4-rust-jit-compiler/Cargo.toml
```

When the loaded image is dropped, including on `UNLOAD`, the diagnostic build
writes one JSON record to standard error. Records use
`schema: "rv32vm.vm4.profile"` and `schema_version: 1` and summarize:

- native and interpreted retirement;
- dispatch, cache, compilation, mapping, and compile-time activity;
- continuation, region, and counted-loop behavior;
- native side exits and sparse interpreted and fallback opcode counts; and
- per-run deltas for the most recent 64 runs.

Standard output and protocol responses are unchanged. Attempted interpreted
instructions include traps, while retired counts do not. Fallback opcode counts
cover instructions unsupported by native lowering.
