# rv32vm Interface

This document defines the host-facing interface shared by RV32IM virtual machine implementations.
Guest-visible machine behavior is defined by [`rv32im-eei.md`](rv32im-eei.md).

An implementation provides an executable named `rv32vm` with two subcommands:
`run` executes one guest in a new process, and `serve` executes repeated requests in one process.
This specification does not define how the executable is built, installed, or isolated.

## 1. One-Shot Command

```text
rv32vm run --elf PROGRAM.elf --input INPUT.bin \
  --output OUTPUT.bin --result RESULT.json \
  [--instruction-limit COUNT] [--output-limit BYTES] \
  [--state STATE.json [--inspect ADDRESS:LENGTH] ...]
```

`--elf`, `--input`, `--output`, and `--result` are required.

Limit values are
unsigned decimal integers:

| Option | Default | Minimum | Maximum |
|---|---:|---:|---:|
| instruction limit | `100,000,000` | `0` | `1,000,000,000` |
| output limit | `1,048,576` | `0` | `1,048,576` |

The ELF file may contain at most 8,388,608 bytes. The input file may contain
at most 4,194,304 bytes; empty input is valid.

This interface does not define `-h`, `--help`, or repeated occurrences of a
single-valued option; implementations may reject them or provide
implementation-specific behavior. Portable callers must not use them.

The destination paths `--output`, `--result`, and `--state`, when present,
must be pairwise non-aliasing: no two may name or resolve to the same file.
Behavior for aliased destination paths is outside this interface. Source
files are read before any destination is replaced, so a destination may alias
`--elf` or `--input`; whether replacing it succeeds depends on its host
filesystem permissions.

Otherwise, unknown, missing, or invalid arguments, inaccessible files,
rejected ELFs, and failure to write any requested file are host errors and
produce a nonzero process status. Guest exit, trap, and instruction-limit
termination are completed runs and produce status zero.

A completed run replaces `OUTPUT.bin` with the guest output and `RESULT.json`
with the result described below. Output written before a trap or instruction
limit is preserved. Existing files are replaced, not appended to.

Replacement of multiple files is not transactional. If a host write error
occurs, a file written earlier may already have been replaced; no rollback is
guaranteed.

Except for the explicitly excluded help and repeated-option cases above,
`run` writes nothing to standard output, including when it terminates with a
host error. Diagnostics may be written to standard error.

### 1.1 Diagnostic State

`--state` additionally replaces `STATE.json` with the machine state after
termination. Each `--inspect ADDRESS:LENGTH` appends one memory range in
command-line order and requires `--state`.

Addresses and lengths use either canonical decimal notation (`0`, or a
nonzero digit followed by digits) or hexadecimal notation with a lowercase
`0x` prefix. Signs, whitespace, uppercase `0X`, and decimal leading zeros are
invalid.

An inspection range must not wrap. Every byte of a nonempty range must be
below `0x0400_0000` and in a mapped page. A zero-length range is valid at any
address through `0x0400_0000`. At most 1,024 ranges may be requested. The sum
of all requested lengths may not exceed 8 MiB; overlapping and repeated bytes
count once for each range that requests them.

The state file is compact UTF-8 JSON with exactly these members, in this order:

```json
{"schema_version":1,"pc":65536,"registers":[0,0,67108864,0,0,0,0,0,0,0,50331648,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"memory":[],"retired_instructions":0,"output_length":0}
```

- `schema_version` is the integer `1`.
- `pc` and the exactly 32 register values are unsigned 32-bit integers.
- `retired_instructions` is an unsigned 64-bit integer.
- `output_length` is an unsigned 32-bit integer equal to the returned output
  length.
- Each memory entry has exactly the form
  `{"address":ADDRESS,"data_base64":"DATA"}`.
- `DATA` contains the requested post-termination memory bytes in canonical
  padded RFC 4648 base64.

The JSON contains no insignificant whitespace or trailing newline.
`--inspect` without `--state`, an invalid inspection range, or failure to
replace the state file is a host error.

## 2. Result JSON

The result is compact UTF-8 JSON with exactly these members, in this order:

```json
{"schema_version":1,"status":"exit","exit_code":0,"trap":null,"resource_failure":null,"retired_instructions":1,"output_length":0}
```

The JSON contains no insignificant whitespace or trailing newline.

| Member | Value |
|---|---|
| `schema_version` | integer `1` |
| `status` | `"exit"`, `"trap"`, or `"resource_failure"` |
| `exit_code` | unsigned 32-bit integer or `null` |
| `trap` | trap object or `null` |
| `resource_failure` | resource-failure object or `null` |
| `retired_instructions` | unsigned 64-bit integer |
| `output_length` | unsigned 32-bit integer |

Exactly one result variant is valid:

| `status` | `exit_code` | `trap` | `resource_failure` |
|---|---|---|---|
| `exit` | integer | `null` | `null` |
| `trap` | `null` | object | `null` |
| `resource_failure` | `null` | `null` | object |

A trap object has exactly these members, in order:

```json
{"cause":"IllegalInstruction","pc":65536,"value":4294967295}
```

`pc` and `value` are unsigned 32-bit integers. The allowed causes and their
meanings are defined in
[`rv32im-eei.md`](rv32im-eei.md#8-traps).

A VM-produced resource failure has exactly this form:

```json
{"cause":"InstructionLimit"}
```

`output_length` must equal the number of output bytes returned separately.
Retirement and termination behavior is defined by the EEI.

## 3. Persistent Command

```text
rv32vm serve
```

`serve` exchanges synchronous binary frames over standard input and standard
output. All integers are unsigned and little-endian. Standard output contains
only protocol bytes; diagnostics may be written to standard error.

Requests are processed and answered in arrival order. A client may send the
next complete request before reading the previous response, but the server
need not execute requests concurrently.

### 3.1 Frame Header

Every request and response begins with this 16-byte header, equivalent to the
structure `<4sBBHII`:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 4 | ASCII magic `RV32` |
| 4 | 1 | protocol version, exactly `1` |
| 5 | 1 | opcode |
| 6 | 2 | request flags or response status |
| 8 | 4 | request ID |
| 12 | 4 | payload length |

A payload may not exceed 8 MiB. Request flags must be zero. The request ID is
opaque and may be any unsigned 32-bit value; the response echoes it unchanged.

Immediately after startup, the server writes and flushes a header with opcode
`0x80`, status `0`, request ID `0`, and no payload. This is the ready frame.

### 3.2 Commands

| Opcode | Command | Request payload |
|---:|---|---|
| `0x01` | `LOAD` | complete ELF bytes |
| `0x02` | `RUN` | limits and input |
| `0x03` | `RESET` | empty |
| `0x04` | `UNLOAD` | empty |
| `0x05` | `SHUTDOWN` | empty |

The response opcode is the request opcode bitwise-ORed with `0x80`.

#### `LOAD`

`LOAD` is valid only when no image is loaded. Its payload must be nonempty and
satisfy the EEI ELF rules. Success installs an immutable image, returns an
empty payload, and enters the loaded state.

A failed `LOAD` leaves the server state unchanged. A rejected ELF in the empty
state therefore leaves the server empty, while a `LOAD` error in the loaded
state retains the existing image.

#### `RUN`

`RUN` is valid only with an image loaded. Its payload begins with this 16-byte
prefix, equivalent to `<QII`:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | instruction limit |
| 8 | 4 | output limit |
| 12 | 4 | input length |

Exactly `input length` bytes follow. The input and limits have the same bounds
as the one-shot command.

Each `RUN` restores the loaded image's initial guest state before installing
the input and executing. Earlier guest memory, registers, output, and
retirement count cannot affect it. Permitted implementation caches may
persist.

A successful response begins with this 8-byte prefix, equivalent to `<II`:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 4 | result JSON length |
| 4 | 4 | output length |

The prefix is followed by exactly the result JSON bytes and then the raw
output bytes. The lengths satisfy
`frame payload length = 8 + result JSON length + output length`, and the framed
output length equals the JSON `output_length`. The server remains loaded.

#### `RESET`

`RESET` is valid only with an image loaded and has no payload. It restores
pristine image memory, zero input, empty output, initial registers and `pc`,
and a zero retirement counter. Image-specific decoded or generated-code
caches may remain. Its successful response is empty, and the server remains
loaded.

#### `UNLOAD`

`UNLOAD` is valid only with an image loaded and has no payload. It removes the
image, guest state, and all image-specific caches. Its successful response is
empty, and the server becomes empty.

#### `SHUTDOWN`

`SHUTDOWN` is valid whether or not an image is loaded and has no payload. The
server writes and flushes an empty successful response, then exits with process
status zero without writing further protocol bytes. End-of-file before a
complete `SHUTDOWN` is a protocol failure.

### 3.3 Response Status

The response header's 16-bit status field has one of these values:

| Code | Name | Exact error payload |
|---:|---|---|
| `0` | `OK` | command-specific |
| `1` | `MalformedFrame` | `malformed frame` |
| `2` | `UnsupportedVersion` | `unsupported version` |
| `3` | `UnknownOpcode` | `unknown opcode` |
| `4` | `InvalidFlags` | `invalid flags` |
| `5` | `FrameTooLarge` | `frame too large` |
| `6` | `InvalidPayload` | `invalid payload` |
| `7` | `InvalidState` | `invalid state` |
| `8` | `ElfRejected` | `ELF rejected` |
| `9` | `InternalError` | `internal error` |

An error response payload contains only the exact UTF-8 text shown in the
table. Guest exit, trap, and instruction-limit termination are successful
`RUN` responses with status `OK`.

For a fully received request, validation order is frame magic and size,
version, flags, opcode, payload structure and bounds, state-machine validity,
then command semantics. A structurally invalid command therefore returns
`InvalidPayload` even when the command would also be invalid in the current
state.

Unsupported version, unknown opcode, invalid flags, invalid payload, invalid
state, and rejected ELF are recoverable. The server returns one error response,
changes no state, and continues reading requests.

Invalid magic, an oversized or truncated frame, and an internal error are
terminal. The server sends the most specific response it can correlate, then
exits nonzero. Every response, including ready, errors, and shutdown, is
flushed promptly.

### 3.4 Lifecycle

```text
start  --READY--------> EMPTY
EMPTY  --LOAD/OK------> LOADED
LOADED --RUN/OK-------> LOADED
LOADED --RESET/OK-----> LOADED
LOADED --UNLOAD/OK----> EMPTY
EMPTY  --SHUTDOWN/OK--> exit
LOADED --SHUTDOWN/OK--> exit
```

Recoverable errors leave the state unchanged.

## 4. Equivalence

For identical ELF bytes, input bytes, instruction limit, and output limit,
`run` and `serve` must produce identical output bytes and semantically
identical result objects. Results must not depend on prior runs, host state, or
retained implementation caches.
