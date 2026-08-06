# VM Performance Benchmarks on GCP

`make gcp-benchmark` runs VM0 through VM5 and the x86-64 native reference on one
disposable Google Compute Engine host. Every implementation uses the same
Linux/amd64 container, harness settings, validated CPU model, one hardware
thread per core, no network, and a 4 GiB memory limit. VM samples measure `RUN`
round trips; native samples measure only the workload function.

## Measurement and reports

- Cases run in manifest order. For each case, the VM0-through-VM5-and-native
  order rotates so no implementation always runs first or last. The raw JSON
  records the schedule; VM0 remains the comparison baseline.
- The detailed table includes every case. The aggregate includes workloads from
  the manifest's `application_workloads` list. It takes a geometric mean within
  each workload, then across workloads so each workload has equal weight.
  Diagnostic workloads remain detailed-only.

## Host reproducibility

The default `official` profile requires the pinned zone, machine, Ubuntu image,
CPU platform, and CPU model. It also verifies one thread per core,
terminate-on-maintenance, and disabled automatic restart. Any mismatch aborts
the run.

The startup script:

- installs exact Docker, containerd, and runc versions from a pinned Ubuntu
  snapshot;
- disables background APT updates and verifies the installed packages and
  Docker service; and
- saves the resolved package versions with the results.

Update the image, Ubuntu snapshot, and package versions together. The host is
deleted after success or failure; a four-hour maximum lifetime handles
interrupted cleanup. Results retain the source revision plus image, instance,
guest CPU, package, and container metadata.

## Prerequisites

The Google Cloud CLI must be authenticated to a project with Compute Engine enabled
and permission to create and delete instances and disks. The configured VPC and
subnet must allow direct SSH or SSH through IAP. The runner does not modify firewall rules.

Create the ignored local configuration:

```sh
cp .env.gcp.example .env.gcp
```

Set `GCP_PROJECT`, network, and subnet. Set `GCP_USE_IAP=1` for IAP, or set
`GCP_NETWORK_TAGS` when required by the SSH firewall rule.

Official runs reject changes to the pinned zone, machine, and image. The
`authoring` profile allows other zones, machines, and concrete Ubuntu Noble
images, but its results are not official. The runner verifies the exact CPU
after creation because `--min-cpu-platform` specifies only a minimum generation.

## Run

Commit the code to measure and use a clean worktree:

```sh
make gcp-benchmark
```

Results are stored below `benchmarks/out/gcp/`.

## Long-horizon run

Run the eight representative application workloads configured in
`long_cases.json` at 10x and 100x with VM4, VM5, and the native reference only:

```sh
make gcp-benchmark-long
```

The standard run is unchanged. This command prints native-relative geometric
means by horizon and stores raw data in `comparison-long.json`. The instruction
limit remains 100 million by default, with a 1 billion maximum.
