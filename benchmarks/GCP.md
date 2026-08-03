# VM Performance Benchmarks on GCP

`make gcp-benchmark` runs VM0 through VM5 and the x86-64 host-native workload reference
on one disposable Google Compute Engine host. It builds one image from a clean repository
revision and runs every implementation in the same Linux/amd64 container with one pinned
CPU, no network, and a 4 GiB memory limit. They share harness settings, but VM samples
time `RUN` round trips while native samples time only the workload function.

Runs execute from VM0 through VM5 sequentially, then native, with VM0 as the baseline.
After the detailed per-workload comparison, the report prints a second table
covering every implementation. It uses geometric means across the six
application workloads (`sha256`, `heatshrink`, `depthconv`, `dijkstra`,
`sort_records`, and `qrcode`) to report speedup over VM0, host-native performance
fraction, and time relative to native. The `tiny`, `arithmetic`, and `streaming`
diagnostics remain visible in the detailed table but do not affect this aggregate.

The host uses one hardware thread per core. It is deleted after results are
copied and after failures; a four-hour maximum lifetime also deletes it if
local cleanup is interrupted. Separate instances can use different physical
hosts, so instance and CPU information is retained with the results.

## Prerequisites

The Google Cloud CLI must be authenticated to a project with Compute Engine enabled
and permission to create and delete instances and disks. The configured VPC and
subnet must allow direct SSH or SSH through IAP. The runner does not modify firewall rules.

Create the ignored local configuration:

```sh
cp .env.gcp.example .env.gcp
```

Set `GCP_PROJECT` and adjust the zone, network, and subnet as needed. Set
`GCP_USE_IAP=1` for IAP or `GCP_NETWORK_TAGS` when the SSH firewall rule
requires instance tags. Keep the machine type and benchmark settings stable
across comparisons.

## Run

Commit the code to measure and use a clean worktree:

```sh
make gcp-benchmark
```

Results are stored below `benchmarks/out/gcp/` with the raw comparison JSON,
source revision, resolved OS image, instance description, and host/container
facts.

## Long-horizon run

Run the application workloads at 10x and 100x with VM4, VM5, and the
native reference only:

```sh
make gcp-benchmark-long
```

The standard run is unchanged. This command prints native-relative geometric
means by horizon and stores raw data in `comparison-long.json`. The instruction
limit remains 100 million by default, with a 1 billion maximum.
