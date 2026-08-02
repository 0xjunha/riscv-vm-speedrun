# RV32IM Rust AOT Compiler

An RV32IM interpreter that compiles supported instruction blocks into native
x86-64 code when an ELF is loaded. This follows the VM interface: `LOAD`
supplies the program, and later `RUN` requests reuse its compiled blocks until
the image is replaced or unloaded.

Supported RV32I/RV32M arithmetic, checked integer loads and stores, and aligned
JALR dispatches run in the eager native image. JALR uses an immutable,
image-scoped two-level table to find already compiled targets without returning
to Rust. A missing target exits only after committing the JALR; a misaligned
target takes the precise one-instruction retry path so link-register and
retirement semantics remain exact. Memory slow paths, syscalls, traps, short
instruction budgets, and unsupported instructions use the shared interpreter.

Translation first follows direct control flow from the image entry, with at
most two successors per admitted block, then scans at most 262,144 file-backed
instructions (1 MiB of RV32 code) for indirect-only and otherwise unreachable
code. It retains at most 8,192 native blocks and their lookup metadata and
stores their page-rounded x86-64 code in one 32 MiB arena. These limits bound
`LOAD` time and memory; code beyond them runs in the interpreter. Indirect
dispatch adds one 128 KiB root plus one 4 KiB leaf per guest page containing a
native entry, plus one owner pointer per leaf, bounded by 32.1875 MiB at the
block cap. VM5 also chooses at most six nonzero guest registers for one
image-global cache in `EBX`, `EBP`, and `R12D` through `R15D`. Selection is
deterministic and image-derived: admitted instruction PCs are deduplicated,
backward conditional and tail-jump intervals receive a generic natural-loop
weight, and static reads/writes are ranked with guest-index tie breaking. A
register must have at least five loop-weighted body accesses, enough to exceed
the fixed push/fill/spill/pop cost of caching it. Its LOAD scratch is bounded
by 524,288 compact instruction records and 16,384 loop events at the block cap.

For a nonempty cache, external entries are grouped in a cold thunk section.
Each begins with `ENDBR64`, identifies its block's separate internal `ENDBR64`,
and jumps to one shared prologue that preserves and fills the cache and loads
the private ABI. Images below the cache-amortization threshold instead keep the
four ABI-anchor loads inline between distinct external and internal `ENDBR64`
pads, avoiding a thunk and shared-prologue round trip. Direct links skip both
landing pads, while native JALR dispatch reaches the internal pad with the
private ABI live. Every exit shares a spill/ABI-restore tail. The code arena is
written before it is changed to read-execute, so it is never writable and
executable at the same time; dispatch data is separate and never executable.

Code admission conservatively reserves cold budget, precise-retry, and missing
target veneers, the larger uncached inline-entry form for every block, and the
worst-case shared six-register prologue/exit (115 bytes normally, 131 bytes
with profiling) before publishing an image. That block charge also bounds the
smaller internal-pad plus 16-byte-thunk form selected for cached images. Final
linking omits direct-edge veneers for native targets, shares unresolved
direct-edge veneers by guest PC, and shares one generic JALR miss veneer across
the image. The optional profiler reports exact finalized hot and cold code,
thunk/prologue/exit (zero thunk and prologue bytes for uncached images), cache,
and dispatch-table sizes.

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

Each record has `kind: "vm5_profile"` and `schema_version: 6`. It reports:

- total, native, and interpreter-fallback retired instructions;
- native invocations, executed blocks, direct linked-edge hits, indirect JALR
  table hits and misses, native exit reasons (including precise
  `interpret_one` memory and JALR exits), and lookup versus short-budget
  fallbacks;
- fallback classes (loads, stores, JALR, M operations, system, other, and fetch
  traps) and a base-opcode breakdown;
- exact generated guest-register array loads and stores, including cache fills
  and spills, plus explicit fill/spill, cached read/write hit, and avoided
  array-access counts;
- successful native memory loads and stores;
- dynamic executions of direct immediate, basic register-ALU, branch, cached
  load-destination, and cached store-source lowering forms; these are counted
  once per executed guest instruction and do not replace the logical
  register-cache traffic fields;
- native fallthrough, conditional-branch, direct-jump, and indirect-jump
  dispatches; and
- LOAD-time compiled block, native guest instruction, raw/hot/cold code byte,
  mapped byte, ordered guest-register cache mapping,
  external-thunk/shared-entry/exit bytes, dispatch-table entry/page/byte, and
  block control-flow counts.

Fallback class counts describe attempted interpreter instructions, so a
trapping instruction appears in its class but not in `retired.fallback`.
`lookup_fallbacks` means no published native entry existed at the current PC;
`budget_fallbacks` means an entry existed but would have crossed the exact
instruction limit. LOAD measurements are repeated unchanged on each RUN of the
same loaded image so every JSON line is self-contained.
