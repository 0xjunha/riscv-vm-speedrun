#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
destination=$repository/jobs/gcp-harbor-selected

command -v gcloud >/dev/null 2>&1 || {
    echo "missing required command: gcloud" >&2
    exit 2
}

mkdir -p "$destination"
gcloud storage rsync \
    gs://selected-harbor-results \
    "$destination" \
    --recursive

echo "Selected Harbor results: $destination"
