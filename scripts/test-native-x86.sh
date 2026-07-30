#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPOSITORY_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
IMAGE=${1:-rv32im-benchmark-builder:local}

if [ "$#" -gt 1 ]; then
    echo "usage: $(basename "$0") [IMAGE]" >&2
    exit 2
fi

docker run --rm --platform linux/amd64 \
    --user "$(id -u):$(id -g)" \
    -e CARGO_TARGET_DIR=/tmp/rv32vm-native-x86-target \
    -v "$REPOSITORY_ROOT:/repo:ro" \
    -w /repo \
    "$IMAGE" \
    sh -eu -c '
        cargo test --locked \
            --manifest-path vm_references/rust-x86-block-compiler/Cargo.toml
        cargo test --locked \
            --manifest-path vm_references/vm4-rust-jit-compiler/Cargo.toml
        cargo test --locked \
            --manifest-path vm_references/vm5-rust-aot-compiler/Cargo.toml
    '
