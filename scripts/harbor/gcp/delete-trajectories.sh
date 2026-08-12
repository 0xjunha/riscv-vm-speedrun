#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
env_file=$repository/.env.gcp.harbor

[ "$#" -gt 0 ] || { echo "usage: $0 ARCHIVE.tgz [...]" >&2; exit 2; }
[ -f "$env_file" ] || { echo "missing $env_file" >&2; exit 2; }
. "$env_file"
: "${GCP_TRAJECTORY_BUCKET:?set GCP_TRAJECTORY_BUCKET in $env_file}"

for archive do
    case "$archive" in
        *.tgz) ;;
        *) echo "invalid trajectory archive: $archive" >&2; exit 2 ;;
    esac
    case "$archive" in
        *[!a-z0-9.-]*) echo "invalid trajectory archive: $archive" >&2; exit 2 ;;
    esac
    gcloud storage rm "gs://$GCP_TRAJECTORY_BUCKET/$archive"
done
