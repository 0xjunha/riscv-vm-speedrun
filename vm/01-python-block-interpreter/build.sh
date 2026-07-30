#!/bin/sh
set -eu

base_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec "$base_dir/../python-interpreter-common/build.sh" "$base_dir"
