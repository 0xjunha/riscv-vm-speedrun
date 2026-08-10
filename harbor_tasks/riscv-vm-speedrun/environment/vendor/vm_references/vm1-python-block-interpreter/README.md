# RV32IM Python Block Interpreter

A standalone Python RV32IM interpreter that decodes instructions in small
blocks on first execution, then caches those blocks for reuse.

The cache contains only decoded instructions and remains available across
`RUN` requests and `RESET`. Every run starts with fresh registers, memory,
input, output, and resource counters. `UNLOAD` discards the loaded image and
its cache.

`build.sh` packages the common Python VM runtime and this directory's cached
execution engine as the standalone `out/rv32vm`.

```sh
make vm1
make vm1-conformance
make vm1-contract
make vm1-benchmark-smoke
```
