# Sail RISC-V Model

This directory contains a pinned copy of the upstream
[Sail RISC-V model](https://github.com/riscv/sail-riscv), a formal
specification adopted by RISC-V International. The model defines instruction
encodings, decoders, and execution semantics.

The project file lists the model sources and their module dependencies:

- [`model/riscv.sail_project`](model/riscv.sail_project)
- [`model/core/`](model/core/): common architectural state and operations
- [`model/extensions/I/`](model/extensions/I/): base integer instructions
- [`model/extensions/M/`](model/extensions/M/): multiplication and division
- [`model/sys/`](model/sys/): memory and execution support

The model contains extensions beyond RV32IM. Only the subset selected by
[`rv32im-eei.md`](../rv32im-eei.md) is part of this task. This copy is provided
as a readable formal reference, not as a standalone simulator build.

See [LICENCE](LICENCE) for the upstream license.
