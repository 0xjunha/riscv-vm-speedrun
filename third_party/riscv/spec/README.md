# RISC-V specifications

This directory contains unmodified, pinned source snapshots used for RV32I and
the M extension:

- The RISC-V ISA manual: [`rv32.adoc`](riscv-isa-manual-v20260120/src/unpriv/rv32.adoc)
  and [`m-st-ext.adoc`](riscv-isa-manual-v20260120/src/unpriv/m-st-ext.adoc).
- The Sail model: [`base_insts.sail`](sail-riscv-0.12/model/extensions/I/base_insts.sail)
  and [`mext_insts.sail`](sail-riscv-0.12/model/extensions/M/mext_insts.sail).

Do not edit the imported snapshots directly. Their exact sources, revisions,
and hashes are recorded in
[`specifications.lock.json`](../manifests/specifications.lock.json) and
[`specifications.inventory.tsv`](../manifests/specifications.inventory.tsv).
Use `scripts/import-riscv-specifications.sh` to create a fresh import and
`scripts/verify-riscv-specifications.sh` to verify one.

Licenses and attribution are recorded in
[`THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md).
