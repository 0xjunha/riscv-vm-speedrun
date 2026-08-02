# RV32IM Rust JIT Compiler

A demand-driven RV32IM interpreter that compiles frequently executed blocks
into native x86-64 code, then forms bounded regions along strongly dominant
cached paths. A unique physical path that closes back to its head is emitted as
a budgeted counted native loop; if loop validation rejects it, VM4 retains the
bounded finite-region fallback.

Checked memory operations and RV32M execute natively. Syscalls, precise native
side exits, short instruction budgets, and unsupported instructions use the
shared interpreter.

The engine starts compiling a cached block on its third complete execution.
After a native head edge is observed at least eight times with at least 7/8
dominance, VM4 follows the dominant native path for up to eight blocks and 256
instructions, and prefers the first head-closing prefix, including a self-loop.
Otherwise it compiles the longest valid prefix of at least two blocks. Counted
loops retain a separate four-block, 128-instruction bound and emit one logical
guest cycle per host-loop iteration. They execute only complete cycles within
the remaining instruction budget and return to the normal dispatcher for an
exact short-budget tail, where the original block entry remains available.
Finished or nondominant bounded profiles stop mutating during steady dispatch.
Basic and region entries share packed executable cohorts and the same exact
mapped-code budget.
The cache is capped at 8,192 blocks, 262,144 decoded instructions, and 16 MiB
of native mappings. It belongs to one loaded ELF and is released by `UNLOAD`.

Executable memory is written first and then changed to read-execute; it is
never writable and executable at the same time.

The VM executable runs only on x86-64 Linux.

## Profile counters

Build with `--features profile` to collect VM4-only diagnostics. When a loaded
image is dropped (including `UNLOAD`), VM4 writes one compact JSON object to
standard error with schema `rv32vm.vm4.profile`. Standard output and protocol
responses are unchanged.

The record includes aggregate native/interpreted retirement, dispatch and
cache activity, compilation outcomes, emitted and mapped code bytes,
compile-and-publish time, native side exits, and sparse interpreted, fallback,
and side-exit opcode counts. Region calls, retirement, completed paths, guard
exits, side exits, budget fallbacks, and compilation outcomes are reported
separately while remaining included in the aggregate native totals. Counted
loop retirement, calls, completed logical cycles, budget completions, guard and
side exits, short-budget fallbacks, compile outcomes, and emitted bytes are a
subset of those region and aggregate counters. Its `recent_runs` array contains
chronological deltas for the most recent 64 runs, which separates initial
tiering from steady-state execution without unbounded memory use.
`interpreted_instructions` counts attempted interpreter execution;
`interpreted_retired` excludes trapping instructions. Fallback opcode counts
are the interpreted instructions unsupported by the native lowering.

Without the feature, the profile state and all recording calls are removed at
compile time.

```sh
make vm4
make vm4-conformance
make vm4-contract
make vm4-benchmark-smoke
make vm4-x86-check
```
