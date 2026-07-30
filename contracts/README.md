# RV32IM VM Contract Tests

These tests verify the project-specific behavior that every VM implementation
must provide:

- [`rv32im-eei.md`](../interface_specs/rv32im-eei.md) defines the machine seen
  by guest programs.
- [`rv32vm-interface.md`](../interface_specs/rv32vm-interface.md) defines the
  one-shot `run` command and persistent `serve` protocol.

ACT4 and `riscv-tests` cover ordinary RV32IM instruction correctness. This
suite covers the ELF, execution-environment, command-line, and protocol rules
defined by this project.

## How It Works

Project-authored assembly programs exercise guest-visible behavior. The build
creates their ELFs and deterministic invalid ELF variants, then records the
artifacts and expected results in a path-independent manifest.

Each executable guest case runs through both VM interfaces, which must return
equivalent results and output. ELF acceptance and rejection are also checked
through both interfaces. One-shot `--state` additionally checks final
registers, `pc`, counters, and requested memory ranges; `serve` does not expose
that state.

Command-line checks invoke `run` directly. Protocol checks exchange raw frames
with `serve`. These checks remain separate from the normal `run_once` and
`VmServer` clients, which intentionally send only valid requests.

Every generated case names the specification section that requires it. Each
procedural check has a stable ID and specification section in
`interface_contracts.py`. A failure case targets one rule, except
validation-order checks that deliberately combine errors.

## What Is Tested

### ELF Loading

The suite checks all accepted and rejected ELF properties defined by the EEI:
headers, load segments, address ranges, alignment, permissions, entry point,
section tables, and forbidden runtime features.

Positive boundary cases ensure the loader is not overly restrictive. Rejected
cases use the smallest suitable valid parent ELF and introduce the intended
violation without accidentally breaking unrelated rules. Invalid ELFs must
fail through `run`; a `LOAD` in the empty `serve` state must return
`ElfRejected` and leave the server empty.

### Execution Environment

Guest programs check:

- initial registers, `pc`, stack, input, BSS, and mapped memory;
- memory permissions, address boundaries, alignment, and fault order;
- exit and output syscalls, including validation order and atomic failure;
- every defined trap cause, value, retirement count, and preserved state;
- instruction and output limits;
- pristine state across repeated runs and changing input;
- observable image isolation across load, unload, and reload; and
- deterministic results for identical ELF bytes, input, and limits.

The suite also covers project-selected instruction behavior that general ISA
tests do not establish, including accepted `FENCE` encodings and rejected
unsupported or reserved encodings.

### One-Shot Interface

Direct command tests check:

- required arguments, numeric syntax, defaults, and limits;
- ELF and input size boundaries;
- source and destination file behavior;
- host errors versus completed guest runs;
- replacement of output, result, and state files;
- absence of standard-output text;
- canonical result and diagnostic-state JSON; and
- inspection syntax, ordering, ranges, count, and total-byte limits.

### Persistent Interface

Raw protocol tests check:

- the ready frame and every header field;
- request IDs, payload boundaries, and ordered pipelined responses;
- `LOAD`, `RUN`, `RESET`, `UNLOAD`, and `SHUTDOWN` lifecycle behavior;
- `RUN` input and limit boundaries;
- response framing and canonical result JSON;
- the specified validation order;
- exact recoverable error responses and unchanged state; and
- terminal malformed-frame behavior, process status, and clean shutdown.

### Interface Agreement

For identical ELF bytes, input, instruction limit, and output limit, `run` and
`serve` must return the same output and equivalent result. State assertions
available only through one-shot `--state` are checked separately.

## Sources and Generated Files

```text
contracts/
├── README.md       # Purpose and organization
├── cases.json      # Authored guest/ELF cases and expected results
├── guest/          # Authored assembly and linker files
├── build.py        # Deterministic artifact builder and validator
├── elf_builder.py  # Minimal ELF32 encoder and rule checker
├── tests/          # Tests for contract artifact generation
└── artifacts/      # Generated ELFs and manifest; do not edit manually
```

`cases.json`, `guest/`, `build.py`, `elf_builder.py`, and `tests/` are
project-authored.
`cases.json` is the editable source for guest and ELF test names,
specification references, inputs, limits, and expected results.

`artifacts/` is generated. Its manifest reproduces the authored case data and
adds artifact paths and hashes. The build validates each parent ELF and each
intentional invalid variant; rejection by one VM alone is not treated as proof
that a generated case is correct.

The build uses the repository's pinned Linux RISC-V toolchain environment.
Artifact generation and VM execution remain separate: `build.py` only builds
and validates assets, while the harness runs them.

Command-line and protocol cases are defined in clearly named harness code
because they require procedural process and frame interactions rather than
buildable guest assets.

## Commands

```sh
make contract-build              # Regenerate ELFs and the manifest in Docker
make contract-check              # Verify generated files without Docker
make contract-reproducible       # Rebuild in Docker and compare every byte
make contract VM=/path/to/rv32vm # Run the suite against a VM
```

`make contract-build` reuses the same pinned Linux toolchain image as the
public conformance build. `make contract-check` and test execution use the
generated artifacts and do not require a compiler.

## Observable Limits

Some implementation details cannot be proven through this black-box interface:

- A later `RUN` restores pristine state itself, so tests can verify `RESET`
  responses and lifecycle behavior but cannot distinguish a full reset from a
  no-op reset.
- Reloading images can detect observable stale state after `UNLOAD`, but
  cannot prove that an unused internal cache was physically deleted.
- Exactly proving the default instruction limit is `100,000,000` would require
  an unnecessarily long run. A default-versus-explicit smoke check verifies
  normal default behavior without claiming that exact value was proven.
- `InternalError` is defined, but no external request is required to trigger
  it.
