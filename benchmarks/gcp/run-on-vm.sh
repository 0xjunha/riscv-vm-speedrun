#!/bin/sh
set -eu

if [ "$#" -ne 6 ]; then
    echo "usage: $0 SOURCE_DIR RESULT_DIR REVISION WARMUPS REPETITIONS TIMEOUT" >&2
    exit 2
fi

source_dir=$1
result_dir=$2
revision=$3
warmups=$4
repetitions=$5
timeout=$6
image="rv32im-benchmark:$revision"
container="rv32im-benchmark-$$"

cleanup() {
    sudo docker rm --force "$container" >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$result_dir"
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
    --warmups "$warmups" \
    --repetitions "$repetitions" \
    --timeout "$timeout" >/dev/null

sudo docker start --attach "$container"
sudo docker cp "$container:/results/comparison.json" \
    "$result_dir/comparison.json"
sudo chown "$(id -u):$(id -g)" "$result_dir/comparison.json"
