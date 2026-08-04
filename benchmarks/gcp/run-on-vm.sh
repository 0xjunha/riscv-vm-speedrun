#!/bin/sh
set -eu

if [ "$#" -ne 7 ]; then
    echo "usage: $0 SOURCE_DIR RESULT_DIR REVISION WARMUPS REPETITIONS TIMEOUT MODE" >&2
    exit 2
fi

source_dir=$1
result_dir=$2
revision=$3
warmups=$4
repetitions=$5
timeout=$6
mode=$7
image="rv32im-benchmark:$revision"
container="rv32im-benchmark-$$"

case "$mode" in
    standard) set -- ;;
    long) set -- --long ;;
    *)
        echo "benchmark mode must be standard or long" >&2
        exit 2
        ;;
esac

cleanup() {
    sudo docker rm --force "$container" >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$result_dir"
cp /var/tmp/rv32im-benchmark-host-packages.txt "$result_dir/host-packages.txt"
cd "$source_dir"
sudo docker build \
    --build-arg "SOURCE_REVISION=$revision" \
    --file benchmarks/gcp/Dockerfile \
    --tag "$image" \
    .

{
    printf 'source revision: %s\n\n' "$revision"
    uname -a
    printf '\n'
    lscpu
    printf '\nhost package contract:\n'
    cat "$result_dir/host-packages.txt"
    printf '\n'
    sudo docker version
    printf '\nbenchmark image: '
    sudo docker image inspect --format '{{.Id}}' "$image"
} >"$result_dir/environment.txt"

sudo docker create \
    --name "$container" \
    --network none \
    --init \
    --cpuset-cpus 0 \
    --memory 4g \
    --pids-limit 256 \
    --security-opt no-new-privileges \
    --cap-drop ALL \
    "$image" \
    "${@}" \
    --warmups "$warmups" \
    --repetitions "$repetitions" \
    --timeout "$timeout" >/dev/null

sudo docker start --attach "$container"
sudo docker cp "$container:/results/." "$result_dir"
sudo chown -R "$(id -u):$(id -g)" "$result_dir"
