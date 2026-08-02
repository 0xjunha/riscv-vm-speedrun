# Shared x86-64 Block Compiler

Native instruction lowering and executable-memory ownership shared by the
Rust JIT and AOT VMs.

The native entry ABI receives the current run's register file, permission
table, and flat 64 MiB guest-memory base. Entries return the next guest PC, the
exact committed prefix length, and whether the engine must interpret the
faulting instruction as a precise side exit. Legal integer operations, JAL/JALR,
branches, loads, stores, and the complete M extension are lowered natively;
trapping memory paths and exceptional division paths leave state unmodified
and side exit. Native scalar memory operations preserve the EEI's alignment,
range, and page-permission checks, then access the checked guest address
directly from the flat base without a sparse-page lookup or allocation exit.

Within each compiled block, frequently reused guest registers are held in a
bounded host-register cache. Bounded blocks and regions use only the three
caller-saved cache hosts. Counted loops may widen that plan with three
callee-saved hosts; only selected hosts are pushed and restored, and a
callee-saved slot must save more traffic than its save/restore pair under the
conservative two-cycle model. Lowerings that reserve one caller-saved host as
scratch still retain five loop cache slots. Dirty values are written back on
every normal return and precise side exit before the host ABI is restored, so
the public entry ABI always observes a canonical guest register file.
`CompiledBlock` reports the modeled uncached and cached register-file accesses
for profiling generated-code quality. Bounded entries use one complete path as
that model; counted loops use two cycles so selection accounts for their
one-time preload and final spill while still admitting registers referenced
only once per cycle.

`CompiledBlock::compile_region` also accepts a bounded sequence of decoded
basic blocks in predicted-path order. The compiler validates each adjacent
branch, direct-jump, or linear edge and keeps one register plan live across the
whole region. A preferred edge remains in native code; a conditional guard
mismatch returns normally with its actual next PC and exact committed count.
Precise faults still side exit before the faulting instruction commits. Regions
are bounded to four blocks and 128 predicted-path instructions, reject repeated
guest PCs, and never continue across JALR or unsupported instructions. Callers
can opt into larger finite policies through `RegionLimits`, up to the generic
hard caps of 16 blocks and 512 instructions, with an explicit finalized-code
bound.

`CompiledBlock::compile_unrolled_region` provides the same four-block,
128-instruction bounds and structural validation for a finite unroll. Unlike
the acyclic API, it permits repeated or overlapping guest PCs so a caller can
materialize a bounded number of loop iterations. Every occurrence is emitted
separately; no unbounded native loop or hidden control-flow edge is introduced.

`CompiledBlock::compile_loop` instead accepts one physical, head-closing guest
cycle. Its one to four blocks and all guest PCs must be unique, its complete
cycle remains bounded to 128 instructions, and the final preferred edge must
return to the first block. The emitter materializes one physical cycle under
one register plan, preload fixed point, loop-counter decrement, and
host backedge. It reports `instruction_count() == minimum_instruction_count()`
and `loop_unroll_factor() == 1`.

`CompiledBlock::compile_grouped_loop` is the explicit opt-in for emitting up to
four logical cycles in one host-loop iteration. Its finalized code, including
deferred exit stubs, is capped at 64 KiB; an invalid factor or oversized body is
rejected rather than changing the ordinary one-copy policy. Generated exits
retain exact retirement in every physical copy. Frequently used guest
registers are loaded once before the group, and every loop-written cached value
is conservatively spilled on all exits.

`NativeEntry::execute_with_limit` is valid for both finite and loop entries. It
returns `None` without mutation when the remaining limit cannot cover the
finite maximum or one complete native loop quantum. Loop budgets are capped
below the outcome side-exit bit; any tail shorter than the published minimum
stays in the VM's exact fallback path.

VM4 publishes lazy blocks individually. VM5 stages blocks in one bounded program.
Each VM owns block formation and decides which compiled prefixes are worthwhile.
This directory does not build a VM by itself.

## Source files

- `lib.rs` defines the public interface and non-x86 platform stubs.
- `lowering.rs` converts supported RV32IM instructions into typed native
  operations.
- `emitter.rs` encodes those operations as x86-64 machine code.
- `memory.rs` publishes finalized code using writable-then-executable Linux
  memory mappings.
- `native.rs` owns executable programs and invokes their block entries.
- `test_support.rs` provides small instruction and machine helpers for tests.
