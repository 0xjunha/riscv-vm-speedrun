#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
env_file=${1:-"$repository/.env.gcp"}
mode=${2:-standard}

if [ "$#" -gt 2 ]; then
    echo "usage: $(basename "$0") [ENV_FILE] [standard|long]" >&2
    exit 2
fi
case "$mode" in
    standard | long) ;;
    *)
        echo "benchmark mode must be standard or long" >&2
        exit 2
        ;;
esac

if [ ! -f "$env_file" ]; then
    echo "missing GCP configuration: $env_file" >&2
    echo "copy .env.gcp.example to .env.gcp and set GCP_PROJECT" >&2
    exit 2
fi

set -a
# This is a trusted, local shell environment file.
. "$env_file"
set +a

official_zone=asia-northeast3-a
official_machine_type=c3-highcpu-4
official_image_project=ubuntu-os-cloud
official_image=ubuntu-2404-noble-amd64-v20260723
official_cpu_platform='Intel Sapphire Rapids'
official_cpu_model='Intel(R) Xeon(R) Platinum 8481C CPU @ 2.70GHz'
readonly \
    official_zone \
    official_machine_type \
    official_image_project \
    official_image \
    official_cpu_platform \
    official_cpu_model

: "${GCP_PROJECT:?set GCP_PROJECT in $env_file}"
: "${GCP_BENCHMARK_PROFILE:=official}"
: "${GCP_ZONE:=$official_zone}"
: "${GCP_NETWORK:=default}"
: "${GCP_SUBNET:=default}"
: "${GCP_NETWORK_TAGS:=}"
: "${GCP_MACHINE_TYPE:=$official_machine_type}"
: "${GCP_IMAGE_PROJECT:=$official_image_project}"
: "${GCP_IMAGE:=$official_image}"
: "${GCP_INSTANCE_PREFIX:=rv32im-bench}"
: "${GCP_USE_IAP:=0}"
: "${BENCHMARK_WARMUPS:=2}"
: "${BENCHMARK_REPETITIONS:=7}"
: "${BENCHMARK_TIMEOUT_SECONDS:=30}"

require_official_value() {
    if [ "$2" != "$3" ]; then
        echo "official GCP benchmark requires $1=$3 (got $2)" >&2
        exit 2
    fi
}

case "$GCP_BENCHMARK_PROFILE" in
    official)
        require_official_value GCP_ZONE "$GCP_ZONE" "$official_zone"
        require_official_value \
            GCP_MACHINE_TYPE "$GCP_MACHINE_TYPE" "$official_machine_type"
        require_official_value \
            GCP_IMAGE_PROJECT "$GCP_IMAGE_PROJECT" "$official_image_project"
        require_official_value GCP_IMAGE "$GCP_IMAGE" "$official_image"
        ;;
    authoring) ;;
    *)
        echo "GCP_BENCHMARK_PROFILE must be official or authoring" >&2
        exit 2
        ;;
esac

case "$GCP_PROJECT" in
    your-project-id)
        echo "replace GCP_PROJECT in $env_file" >&2
        exit 2
        ;;
esac
if ! printf '%s\n' "$GCP_INSTANCE_PREFIX" |
    grep -Eq '^[a-z]([-a-z0-9]*[a-z0-9])?$'; then
    echo "GCP_INSTANCE_PREFIX must be a lowercase GCP resource name" >&2
    exit 2
fi
if [ "${#GCP_INSTANCE_PREFIX}" -gt 39 ]; then
    echo "GCP_INSTANCE_PREFIX must not exceed 39 characters" >&2
    exit 2
fi
for value in "$BENCHMARK_WARMUPS" "$BENCHMARK_REPETITIONS"; do
    case "$value" in
        '' | *[!0-9]*)
            echo "benchmark warmups and repetitions must be integers" >&2
            exit 2
            ;;
    esac
done
if [ "$BENCHMARK_REPETITIONS" -eq 0 ]; then
    echo "BENCHMARK_REPETITIONS must be positive" >&2
    exit 2
fi
if ! awk -v value="$BENCHMARK_TIMEOUT_SECONDS" 'BEGIN {
    valid = value ~ /^[0-9]+([.][0-9]+)?$/ && value + 0 > 0
    exit !valid
}'; then
    echo "BENCHMARK_TIMEOUT_SECONDS must be a positive number" >&2
    exit 2
fi
case "$GCP_USE_IAP" in
    0) iap_flag= ;;
    1) iap_flag=--tunnel-through-iap ;;
    *)
        echo "GCP_USE_IAP must be 0 or 1" >&2
        exit 2
        ;;
esac
if [ -n "$GCP_NETWORK_TAGS" ]; then
    tags_flag="--tags=$GCP_NETWORK_TAGS"
else
    tags_flag=
fi

for command in awk gcloud git python3; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "required command not found: $command" >&2
        exit 2
    fi
done

cd "$repository"
if [ -n "$(git status --porcelain)" ]; then
    echo "GCP benchmarks require a clean worktree" >&2
    exit 2
fi
PYTHONDONTWRITEBYTECODE=1 python3 benchmarks/build.py check

revision=$(git rev-parse HEAD)
timestamp=$(date -u +%Y%m%d-%H%M%S)
instance="$GCP_INSTANCE_PREFIX-$timestamp-$$"
result_dir="$repository/benchmarks/out/gcp/$instance"
temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/rv32im-gcp.XXXXXX")
archive="$temporary_dir/source.tar.gz"
remote_archive="/tmp/$instance.tar.gz"
remote_root="/tmp/$instance"
network_interface="network=$GCP_NETWORK,subnet=$GCP_SUBNET,nic-type=GVNIC,stack-type=IPV4_ONLY"
delete_instance=false

cleanup() {
    status=$?
    trap - EXIT
    if [ "$delete_instance" = true ]; then
        if ! gcloud compute instances delete "$instance" \
            --project="$GCP_PROJECT" \
            --zone="$GCP_ZONE" \
            --quiet; then
            echo "warning: could not ensure deletion of GCP instance $instance" >&2
            if [ "$status" -eq 0 ]; then
                status=1
            fi
        fi
    fi
    rm -rf "$temporary_dir"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$result_dir"
printf '%s\n' "$revision" >"$result_dir/source-revision.txt"
git archive --format=tar.gz --output="$archive" HEAD

gcloud compute images describe "$GCP_IMAGE" \
    --project="$GCP_IMAGE_PROJECT" \
    --format=json >"$result_dir/source-image.json"
printf '%s/%s\n' "$GCP_IMAGE_PROJECT" "$GCP_IMAGE" >"$result_dir/source-image.txt"
{
    printf 'benchmark_profile=%s\n' "$GCP_BENCHMARK_PROFILE"
    printf 'zone=%s\n' "$GCP_ZONE"
    printf 'machine_type=%s\n' "$GCP_MACHINE_TYPE"
    printf 'image=%s/%s\n' "$GCP_IMAGE_PROJECT" "$GCP_IMAGE"
    printf 'threads_per_core=1\n'
    printf 'maintenance_policy=TERMINATE\n'
    printf 'automatic_restart=false\n'
    if [ "$GCP_BENCHMARK_PROFILE" = official ]; then
        printf 'expected_cpu_platform=%s\n' "$official_cpu_platform"
        printf 'expected_cpu_model=%s\n' "$official_cpu_model"
    fi
} >"$result_dir/host-contract.txt"

if gcloud compute instances describe "$instance" \
    --project="$GCP_PROJECT" \
    --zone="$GCP_ZONE" >/dev/null 2>&1; then
    echo "refusing to reuse existing GCP instance $instance" >&2
    exit 1
fi

echo "Creating $instance in $GCP_ZONE"
delete_instance=true
gcloud compute instances create "$instance" \
    --project="$GCP_PROJECT" \
    --zone="$GCP_ZONE" \
    --machine-type="$GCP_MACHINE_TYPE" \
    --provisioning-model=STANDARD \
    --maintenance-policy=TERMINATE \
    --no-restart-on-failure \
    --threads-per-core=1 \
    --image="$GCP_IMAGE" \
    --image-project="$GCP_IMAGE_PROJECT" \
    --boot-disk-size=100GB \
    --boot-disk-type=hyperdisk-balanced \
    --boot-disk-auto-delete \
    --network-interface="$network_interface" \
    ${tags_flag:+"$tags_flag"} \
    --no-service-account \
    --no-scopes \
    --metadata-from-file="startup-script=$repository/benchmarks/gcp/startup.sh" \
    --labels=managed-by=rv32im-bench,purpose=benchmark \
    --max-run-duration=4h \
    --instance-termination-action=DELETE \
    --shielded-secure-boot \
    --shielded-vtpm \
    --shielded-integrity-monitoring \
    --quiet

gcloud compute instances describe "$instance" \
    --project="$GCP_PROJECT" \
    --zone="$GCP_ZONE" \
    --format=json >"$result_dir/instance.json"

if [ "$GCP_BENCHMARK_PROFILE" = official ]; then
    python3 benchmarks/gcp/validate_host.py instance \
        "$result_dir/instance.json" \
        "$official_zone" \
        "$official_machine_type" \
        "$official_cpu_platform"
fi

echo "Waiting for Docker"
deadline=$(($(date +%s) + 600))
while ! gcloud compute ssh "$instance" \
    --project="$GCP_PROJECT" \
    --zone="$GCP_ZONE" \
    ${iap_flag:+"$iap_flag"} \
    --ssh-flag=-oConnectTimeout=10 \
    --command='test -f /var/tmp/rv32im-benchmark-ready' \
    --quiet >/dev/null 2>&1; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
        echo "instance startup did not finish within 600 seconds" >&2
        gcloud compute instances get-serial-port-output "$instance" \
            --project="$GCP_PROJECT" \
            --zone="$GCP_ZONE" >&2 || true
        exit 1
    fi
    sleep 5
done

gcloud compute ssh "$instance" \
    --project="$GCP_PROJECT" \
    --zone="$GCP_ZONE" \
    ${iap_flag:+"$iap_flag"} \
    --command='LC_ALL=C lscpu --json' \
    --quiet >"$result_dir/host-lscpu.json"

python3 benchmarks/gcp/validate_host.py cpu \
    "$result_dir/host-lscpu.json" \
    "$GCP_BENCHMARK_PROFILE" \
    "$official_cpu_model"

gcloud compute scp "$archive" "$instance:$remote_archive" \
    --project="$GCP_PROJECT" \
    --zone="$GCP_ZONE" \
    ${iap_flag:+"$iap_flag"} \
    --quiet

gcloud compute ssh "$instance" \
    --project="$GCP_PROJECT" \
    --zone="$GCP_ZONE" \
    ${iap_flag:+"$iap_flag"} \
    --command="mkdir -p '$remote_root/source' '$remote_root/results' &&
        tar -xzf '$remote_archive' -C '$remote_root/source' &&
        '$remote_root/source/benchmarks/gcp/run-on-vm.sh' \
            '$remote_root/source' '$remote_root/results' '$revision' \
            '$BENCHMARK_WARMUPS' '$BENCHMARK_REPETITIONS' \
            '$BENCHMARK_TIMEOUT_SECONDS' '$mode'" \
    --quiet

gcloud compute scp --recurse "$instance:$remote_root/results" "$result_dir" \
    --project="$GCP_PROJECT" \
    --zone="$GCP_ZONE" \
    ${iap_flag:+"$iap_flag"} \
    --quiet

echo "GCP benchmark results: $result_dir/results"
