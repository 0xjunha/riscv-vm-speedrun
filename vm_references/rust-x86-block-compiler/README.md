# Shared x86-64 Block Compiler

Native instruction lowering and executable-memory ownership shared by the
Rust JIT and AOT VMs.

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
