#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 VARIANT_DIR" >&2
    exit 2
fi

common_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
variant_dir=$(CDPATH= cd -- "$1" && pwd)
out_dir="$variant_dir/out"

rm -rf "$out_dir"
mkdir -p "$out_dir/rv32vm_pkg"
cp "$common_dir/src/rv32vm_launcher.py" "$out_dir/rv32vm"
for source in "$common_dir"/src/rv32vm_pkg/*.py; do
    cp "$source" "$out_dir/rv32vm_pkg/"
done
for source in "$variant_dir"/src/rv32vm_pkg/*.py; do
    cp "$source" "$out_dir/rv32vm_pkg/"
done
chmod 0755 "$out_dir/rv32vm"
