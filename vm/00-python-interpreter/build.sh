#!/bin/sh
set -eu

base_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
out_dir="$base_dir/out"

rm -rf "$out_dir"
mkdir -p "$out_dir/rv32vm_pkg"
cp "$base_dir/src/rv32vm_launcher.py" "$out_dir/rv32vm"
for source in "$base_dir"/src/rv32vm_pkg/*.py; do
    cp "$source" "$out_dir/rv32vm_pkg/"
done
chmod 0755 "$out_dir/rv32vm"
