#!/bin/sh
set -eu

base_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
target_dir=${CARGO_TARGET_DIR:-"$base_dir/target"}

CARGO_TARGET_DIR="$target_dir" cargo build --locked --release \
    --manifest-path "$base_dir/Cargo.toml"
mkdir -p "$base_dir/out"
install -m 0755 "$target_dir/release/rv32vm" "$base_dir/out/rv32vm"
