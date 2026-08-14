#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository=$(CDPATH= cd -- "$script_dir/../../.." && pwd)
run_script=$script_dir/run.sh
build_config=$script_dir/cloudbuild.yaml
env_file=$repository/.env.gcp.harbor
agent=codex
starting_vm=
timeout_multiplier=
keep=false
dry_run=false

usage() {
    echo "usage: $0 [--agent AGENT] [--starting-vm VM] [--timeout-multiplier N] [--keep] [--dry-run] [MODEL ...]" >&2
    exit "${1:-2}"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --agent) [ "$#" -ge 2 ] || usage; agent=$2; shift 2 ;;
        --starting-vm) [ "$#" -ge 2 ] || usage; starting_vm=$2; shift 2 ;;
        --env-file) [ "$#" -ge 2 ] || usage; env_file=$2; shift 2 ;;
        --timeout-multiplier) [ "$#" -ge 2 ] || usage; timeout_multiplier=$2; shift 2 ;;
        --keep) keep=true; shift ;;
        --dry-run) dry_run=true; shift ;;
        --help) usage 0 ;;
        --) shift; break ;;
        -*) usage ;;
        *) break ;;
    esac
done
models=$*

[ -f "$env_file" ] || {
    echo "missing $env_file; copy .env.gcp.harbor.example and set GCP_PROJECT" >&2
    exit 2
}
env_file=$(CDPATH= cd -- "$(dirname -- "$env_file")" && pwd)/$(basename -- "$env_file")
set -a
. "$env_file"
set +a

: "${GCP_PROJECT:?set GCP_PROJECT in $env_file}"
: "${GCP_CLOUD_BUILD_REGION:=global}"
: "${GCP_HARBOR_CONTROLLER_SERVICE_ACCOUNT:?set GCP_HARBOR_CONTROLLER_SERVICE_ACCOUNT in $env_file}"
: "${GCP_CODEX_AUTH_SECRET:=}"
: "${GCP_TRAJECTORY_BUCKET:=}"
: "${GCP_MAX_RUN_DURATION:=12h}"
: "${CODEX_AUTH_JSON:=$HOME/.codex/auth.json}"

case "$GCP_CLOUD_BUILD_REGION" in
    *[!a-z0-9-]*) echo "invalid GCP_CLOUD_BUILD_REGION" >&2; exit 2 ;;
esac
case "$GCP_HARBOR_CONTROLLER_SERVICE_ACCOUNT" in
    *[!A-Za-z0-9._@-]*) echo "invalid GCP_HARBOR_CONTROLLER_SERVICE_ACCOUNT" >&2; exit 2 ;;
esac
case "$GCP_HARBOR_CONTROLLER_SERVICE_ACCOUNT" in
    *@*.iam.gserviceaccount.com) ;;
    *) echo "GCP_HARBOR_CONTROLLER_SERVICE_ACCOUNT must be a service-account email" >&2; exit 2 ;;
esac
controller_service_account="projects/$GCP_PROJECT/serviceAccounts/$GCP_HARBOR_CONTROLLER_SERVICE_ACCOUNT"
case "$GCP_CODEX_AUTH_SECRET" in
    *[!A-Za-z0-9_-]*) echo "invalid GCP_CODEX_AUTH_SECRET" >&2; exit 2 ;;
esac
[ -n "$GCP_TRAJECTORY_BUCKET" ] || {
    echo "set GCP_TRAJECTORY_BUCKET so asynchronous results are durable" >&2
    exit 2
}
if [ "$agent" = codex ]; then
    [ -n "$GCP_CODEX_AUTH_SECRET" ] || {
        echo "set GCP_CODEX_AUTH_SECRET for asynchronous Codex runs" >&2
        exit 2
    }
fi
case "$GCP_MAX_RUN_DURATION" in
    *s) run_multiplier=1; run_value=${GCP_MAX_RUN_DURATION%s} ;;
    *m) run_multiplier=60; run_value=${GCP_MAX_RUN_DURATION%m} ;;
    *h) run_multiplier=3600; run_value=${GCP_MAX_RUN_DURATION%h} ;;
    *d) run_multiplier=86400; run_value=${GCP_MAX_RUN_DURATION%d} ;;
    *) echo "GCP_MAX_RUN_DURATION must end in s, m, h, or d" >&2; exit 2 ;;
esac
case "$run_value" in ''|*[!0-9]*) echo "invalid GCP_MAX_RUN_DURATION" >&2; exit 2 ;; esac
run_duration_seconds=$((run_value * run_multiplier))
[ "$run_duration_seconds" -le 82800 ] || {
    echo "asynchronous GCP_MAX_RUN_DURATION cannot exceed 23h" >&2
    exit 2
}

for command in gcloud git tar; do
    command -v "$command" >/dev/null || { echo "missing command: $command" >&2; exit 2; }
done

cd "$repository"
[ -z "$(git status --porcelain)" ] || {
    echo "worktree is dirty; commit or stash changes before submitting to GCP" >&2
    exit 2
}
revision=$(git rev-parse HEAD)
temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/rv32im-harbor-submit.XXXXXX")
secret_version=__none__
build_submitted=false
cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    if [ "$build_submitted" = false ] && [ "$secret_version" != __none__ ]; then
        gcloud secrets versions destroy "$secret_version" \
            --secret="$GCP_CODEX_AUTH_SECRET" --project="$GCP_PROJECT" \
            --quiet >/dev/null 2>&1 || true
    fi
    rm -rf "$temporary_dir"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

source_archive=$temporary_dir/source.tar.gz
invocation=$temporary_dir/invocation
git archive --format=tar.gz --output="$source_archive" HEAD
{
    printf '%s\n' --agent "$agent"
    [ -z "$starting_vm" ] || printf '%s\n' --starting-vm "$starting_vm"
    [ -z "$timeout_multiplier" ] || \
        printf '%s\n' --timeout-multiplier "$timeout_multiplier"
    [ "$keep" = false ] || printf '%s\n' --keep
    printf '%s\n' --
    for model in $models; do
        printf '%s\n' "$model"
    done
} >"$invocation"

preflight() {
    set -- --env-file "$env_file" \
        --source-archive "$source_archive" --source-revision "$revision" --dry-run
    while IFS= read -r argument; do
        set -- "$@" "$argument"
    done <"$invocation"
    "$run_script" "$@"
}

if [ "$dry_run" = true ]; then
    preflight
    printf '%-18s %s\n' \
        controller "Cloud Build ($GCP_CLOUD_BUILD_REGION)" \
        service_account "$GCP_HARBOR_CONTROLLER_SERVICE_ACCOUNT" \
        auth_secret "${GCP_CODEX_AUTH_SECRET:-<not required>}"
    exit 0
fi
preflight >/dev/null

context=$temporary_dir/context
context_archive=$temporary_dir/cloud-build-source.tar.gz
mkdir -p "$context"
tar -xzf "$source_archive" -C "$context"
cp "$source_archive" "$context/.harbor-source.tar.gz"
cp "$env_file" "$context/.env.gcp.harbor"
cp "$invocation" "$context/.harbor-invocation"
printf '%s\n' "$revision" >"$context/.harbor-source-revision"
tar -czf "$context_archive" -C "$context" .

if [ "$agent" = codex ]; then
    secret_version_name=$(gcloud secrets versions add "$GCP_CODEX_AUTH_SECRET" \
        --project="$GCP_PROJECT" --data-file="$CODEX_AUTH_JSON" \
        --format='value(name)' --quiet)
    secret_version=${secret_version_name##*/}
    gcloud secrets versions destroy "$secret_version" \
        --secret="$GCP_CODEX_AUTH_SECRET" --project="$GCP_PROJECT" --quiet
    scheduled_destroy=$(gcloud secrets versions describe "$secret_version" \
        --secret="$GCP_CODEX_AUTH_SECRET" --project="$GCP_PROJECT" \
        --format='value(scheduledDestroyTime)' --quiet)
    [ -n "$scheduled_destroy" ] || {
        echo "configure $GCP_CODEX_AUTH_SECRET with --version-destroy-ttl=1d" >&2
        exit 2
    }
fi

substitutions=$(printf '%s' \
    "_CODEX_AUTH_SECRET=${GCP_CODEX_AUTH_SECRET:-__none__}" \
    ",_CODEX_AUTH_SECRET_VERSION=$secret_version")

build_id=$(gcloud builds submit "$context_archive" \
    --project="$GCP_PROJECT" --region="$GCP_CLOUD_BUILD_REGION" \
    --config="$build_config" \
    --service-account="$controller_service_account" \
    --substitutions="$substitutions" --async --format='value(id)' --quiet)
build_submitted=true

echo "submitted Harbor controller: $build_id"
echo "status: gcloud builds describe $build_id --project=$GCP_PROJECT --region=$GCP_CLOUD_BUILD_REGION"
echo "logs:   gcloud builds log $build_id --project=$GCP_PROJECT --region=$GCP_CLOUD_BUILD_REGION --stream"
