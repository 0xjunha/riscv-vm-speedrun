# Shared Python VM Runtime

This directory contains the host interface, ELF loader, memory model, and
protocol shared by the Python VM variants. It is not a VM by itself:
`build.sh` combines these files with a variant's `rv32vm_pkg/machine.py` to
produce a standalone `out/rv32vm`.
