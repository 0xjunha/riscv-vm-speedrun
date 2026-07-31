#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
env_file=${1:-"$repository/.env.gcp"}

if [ ! -f "$env_file" ]; then
    echo "missing GCP configuration: $env_file" >&2
    echo "copy .env.gcp.example to .env.gcp and set GCP_PROJECT" >&2
    exit 2
fi

set -a
# This is a trusted, local shell environment file.
. "$env_file"
set +a

: "${GCP_PROJECT:?set GCP_PROJECT in $env_file}"
: "${GCP_ZONE:=asia-northeast3-a}"
: "${GCP_NETWORK:=default}"
: "${GCP_SUBNET:=default}"
: "${GCP_NETWORK_TAGS:=}"
: "${GCP_MACHINE_TYPE:=c3-highcpu-4}"
: "${GCP_IMAGE_PROJECT:=ubuntu-os-cloud}"
: "${GCP_IMAGE_FAMILY:=ubuntu-2404-lts-amd64}"
: "${GCP_INSTANCE_PREFIX:=rv32im-bench}"
: "${GCP_USE_IAP:=0}"
: "${BENCHMARK_WARMUPS:=2}"
: "${BENCHMARK_REPETITIONS:=7}"
: "${BENCHMARK_TIMEOUT_SECONDS:=10}"

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

for command in awk gcloud git; do
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

image=$(gcloud compute images describe-from-family "$GCP_IMAGE_FAMILY" \
    --project="$GCP_IMAGE_PROJECT" \
    --format='value(name)')
if [ -z "$image" ]; then
    echo "could not resolve GCP image family $GCP_IMAGE_FAMILY" >&2
    exit 1
fi
printf '%s/%s\n' "$GCP_IMAGE_PROJECT" "$image" >"$result_dir/source-image.txt"

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
    --threads-per-core=1 \
    --image="$image" \
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
            '$BENCHMARK_TIMEOUT_SECONDS'" \
    --quiet

gcloud compute scp --recurse "$instance:$remote_root/results" "$result_dir" \
    --project="$GCP_PROJECT" \
    --zone="$GCP_ZONE" \
    ${iap_flag:+"$iap_flag"} \
    --quiet

echo "GCP benchmark result: $result_dir/results/comparison.json"
