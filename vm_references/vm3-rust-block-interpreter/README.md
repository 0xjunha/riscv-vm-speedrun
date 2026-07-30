# RV32IM Rust Block Interpreter

A Rust RV32IM interpreter that caches decoded instruction blocks instead of
fetching and decoding every instruction each time it runs.

Each block contains up to 64 instructions and stays within one 4 KiB memory page.

The bounded cache is reused across `RUN` and `RESET`, then discarded by
`UNLOAD`; blocks still run without being stored when the cache is full.

```sh
make vm3
make vm3-conformance
make vm3-contract
make vm3-benchmark-smoke
```
