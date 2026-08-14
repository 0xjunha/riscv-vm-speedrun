#!/bin/sh
set -eu

workspace=${HARBOR_WORKSPACE:-/workspace}
run_script=${HARBOR_RUN_SCRIPT:-$workspace/scripts/harbor/gcp/run.sh}
env_file=${HARBOR_ENV_FILE:-$workspace/.env.gcp.harbor}
source_archive=${HARBOR_SOURCE_ARCHIVE:-$workspace/.harbor-source.tar.gz}
source_revision_file=${HARBOR_SOURCE_REVISION_FILE:-$workspace/.harbor-source-revision}
invocation_file=${HARBOR_INVOCATION_FILE:-$workspace/.harbor-invocation}

: "${GCP_PROJECT:?missing GCP_PROJECT}"
: "${HARBOR_RUN_NAMESPACE:?missing HARBOR_RUN_NAMESPACE}"
: "${HARBOR_CODEX_AUTH_SECRET:?missing HARBOR_CODEX_AUTH_SECRET}"
: "${HARBOR_CODEX_AUTH_SECRET_VERSION:?missing HARBOR_CODEX_AUTH_SECRET_VERSION}"
[ -f "$invocation_file" ] || { echo "missing $invocation_file" >&2; exit 2; }
[ -f "$source_revision_file" ] || { echo "missing $source_revision_file" >&2; exit 2; }
source_revision=$(sed -n '1p' "$source_revision_file")
[ -n "$source_revision" ] || { echo "empty $source_revision_file" >&2; exit 2; }

auth_file=
secret_cleanup_pending=false
cleanup() {
    if [ "$secret_cleanup_pending" = true ]; then
        gcloud secrets versions disable "$HARBOR_CODEX_AUTH_SECRET_VERSION" \
            --secret="$HARBOR_CODEX_AUTH_SECRET" --project="$GCP_PROJECT" \
            --quiet >/dev/null 2>&1 || true
        gcloud secrets versions destroy "$HARBOR_CODEX_AUTH_SECRET_VERSION" \
            --secret="$HARBOR_CODEX_AUTH_SECRET" --project="$GCP_PROJECT" \
            --quiet >/dev/null 2>&1 || true
    fi
    [ -z "$auth_file" ] || rm -f "$auth_file"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

if [ "$HARBOR_CODEX_AUTH_SECRET_VERSION" != __none__ ]; then
    auth_file=$(mktemp)
    chmod 0600 "$auth_file"
    secret_cleanup_pending=true
    gcloud secrets versions enable "$HARBOR_CODEX_AUTH_SECRET_VERSION" \
        --secret="$HARBOR_CODEX_AUTH_SECRET" --project="$GCP_PROJECT" --quiet
    gcloud secrets versions access "$HARBOR_CODEX_AUTH_SECRET_VERSION" \
        --secret="$HARBOR_CODEX_AUTH_SECRET" --project="$GCP_PROJECT" >"$auth_file"
    gcloud secrets versions disable "$HARBOR_CODEX_AUTH_SECRET_VERSION" \
        --secret="$HARBOR_CODEX_AUTH_SECRET" --project="$GCP_PROJECT" --quiet
    gcloud secrets versions destroy "$HARBOR_CODEX_AUTH_SECRET_VERSION" \
        --secret="$HARBOR_CODEX_AUTH_SECRET" --project="$GCP_PROJECT" --quiet
    secret_cleanup_pending=false
fi

set -- \
    --env-file "$env_file" \
    --source-archive "$source_archive" \
    --source-revision "$source_revision" \
    --run-namespace "$HARBOR_RUN_NAMESPACE" \
    --upload-results
[ -z "$auth_file" ] || set -- "$@" --auth-file "$auth_file"
while IFS= read -r argument; do
    set -- "$@" "$argument"
done <"$invocation_file"

status=0
"$run_script" "$@" || status=$?
exit "$status"
