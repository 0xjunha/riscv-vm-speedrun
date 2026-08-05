# Embench 1.0 sources

- Repository: https://github.com/embench/embench-iot
- Tag: `embench-1.0`
- Commit: `0466a18e4f6b47e19598d7c6ba72916d54b68f65`
- License: GPL-3.0-or-later; see
  `benchmarks/guest/licenses/Embench-GPL-3.0-or-later.txt`

The files below are copied without modification from the tagged source:

- `src/aha-mont64/mont64.c`
- `src/nettle-aes/nettle-aes.c`
- `src/picojpeg/libpicojpeg.c`
- `src/picojpeg/picojpeg.h`
- `src/sglib-combined/sglib.h`
- `src/slre/libslre.c`
- `src/slre/slre.h`
- `src/statemate/libstatemate.c`
- `src/ud/libud.c`

Project-owned adapters and the small freestanding compatibility header live
under `benchmarks/guest/workloads/c/`; they are not upstream Embench files.
