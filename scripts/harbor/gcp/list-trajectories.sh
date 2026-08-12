#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
env_file=$repository/.env.gcp.harbor

[ -f "$env_file" ] || { echo "missing $env_file" >&2; exit 2; }
. "$env_file"
: "${GCP_TRAJECTORY_BUCKET:?set GCP_TRAJECTORY_BUCKET in $env_file}"

gcloud storage ls -l "gs://$GCP_TRAJECTORY_BUCKET/"
