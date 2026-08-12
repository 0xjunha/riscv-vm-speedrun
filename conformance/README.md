# RV32IM Conformance Inputs

This directory builds 95 public, self-checking ELF inputs:

- 47 ACT4 tests: 39 RV32I and 8 RV32M
- 48 `riscv-tests`: 40 RV32I and 8 RV32M

## Provenance and Pipeline

The untouched upstream sources live under `third_party/riscv/conformance/`.
`scripts/riscv/verify-conformance.sh` checks their pinned inventory and hashes.

For ACT4, `build.py` reruns the imported upstream generator from the imported
test plans and requires all 47 generated `.S` files to match the imported files
byte-for-byte. It then:

1. builds a reference ELF;
2. runs it on pinned Sail to obtain expected memory values;
3. stores those values in `artifacts/reference-results/act4/`; and
4. embeds them in a final self-checking ELF.

For `riscv-tests`, `build.py` compiles the selected upstream `.S` files
directly. These tests already check their own results.

`adapters/` defines project-specific adapters. Its headers report pass or failure through
EEI syscall 0, and its linker scripts place code and data in the EEI image
area.

`patches/` changes only a temporary copy of the ACT4 environment:

- privileged-only failure diagnostics are omitted because this EEI has no
  privilege modes or CSRs;
- compressed alignment instructions are disabled because the target is RV32IM.

The upstream snapshots are never modified.

## How Tests Are Judged

Each ELF is a self-checking program. `riscv-tests` already compares actual
results with expected values. For ACT4, the build runs a reference ELF on Sail
and embeds the resulting trusted values using ACT4's self-check mode.

The runner executes every ELF with empty input through both VM interfaces. The
ELF reports success through the EEI exit syscall. A case passes only if the VM
exits normally with code 0 and produces no output.

The ACT4 `reference-results/` files are build inputs and reproducibility evidence;
the runner does not compare them at runtime.

## Builder and Outputs

The canonical builder is `linux/amd64`. `toolchain.env` pins its Ubuntu image
and package snapshot, GNU RISC-V toolchain source, GCC/Binutils versions, Sail
release version, and release archive checksum. `Dockerfile` builds and verifies
those tools. The pinned Sail binary targets Linux/x86-64, and native toolchains
need not produce identical bytes.

Run:

```sh
make conformance-build          # generate all outputs
make conformance-check          # verify sources, manifest, and hashes
make conformance-reproducible   # rebuild and byte-compare every output
make conformance VM=path/to/vm  # run every ELF through both VM interfaces
```

The first build compiles the pinned GNU toolchain and can be slow under CPU
emulation.
`artifacts/` contains reproducible build outputs and should not be edited
manually. Its `elf/` directory contains the runnable inputs, while
`manifest.json` records every source, ELF, ACT4 reference result, project build
input, and SHA-256 digest. Each ELF should terminate with exit code 0 and
produce no VM-managed output.
