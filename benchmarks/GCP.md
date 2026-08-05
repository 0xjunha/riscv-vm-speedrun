# VM Performance Benchmarks on GCP

`make gcp-benchmark` runs VM0 through VM5 and the x86-64 host-native workload reference
on one disposable Google Compute Engine host. It builds one image from a clean repository
revision and runs every implementation in the same Linux/amd64 container with one pinned
CPU, no network, and a 4 GiB memory limit. They share harness settings, but VM samples
time `RUN` round trips while native samples time only the workload function.

Cases execute in manifest order. Within each case, the complete participant
order (VM0 through VM5 and native) rotates left by the case index, so no
implementation systematically runs first or last. The raw comparison JSON
records this schedule. VM0 remains the comparison baseline regardless of
measurement order.
After the detailed per-case comparison, the report prints a second table
covering every implementation. It selects every case whose workload appears in
the manifest's top-level `application_workloads` list. For workloads with
multiple cases, it first takes the geometric mean across that workload's cases,
then takes the geometric mean across workload results so that every workload has
equal weight. The table reports speedup over VM0, host-native performance fraction,
and time relative to native. The `tiny`, `arithmetic`, and `streaming` diagnostics
remain visible in the detailed table but do not affect this aggregate.

The default `official` profile pins `c3-highcpu-4` in `asia-northeast3-a` and
the concrete Ubuntu image named in `.env.gcp.example`. Google maps C3 to the
Intel Xeon Platinum 8481C (Sapphire Rapids). Before uploading the source, the
runner verifies the returned Compute Engine CPU platform, machine type, zone,
one-thread-per-core setting, exact guest-visible CPU model, terminate-on-host-
maintenance policy, and disabled automatic restart. A mismatch fails the run
rather than producing benchmark results. The default per-operation deadline is
30 seconds so the slow VM0 reference can complete the capacity-boundary QR case.

Host provisioning is pinned too. The startup script uses Canonical's Ubuntu
archive snapshot `20260723T000000Z` through an isolated APT source and package-index
directory, and installs exact `docker.io`, `containerd`, and `runc` versions.
Background APT timers are disabled before installation, and startup reaches its
ready marker only after the installed versions and Docker service are verified.
The snapshot and resolved versions are retained as `host-packages.txt` with every
run. When the host image is updated, review and advance the snapshot and package
versions together, including the security pocket. See Canonical's
[Ubuntu snapshot service documentation](https://documentation.ubuntu.com/server/how-to/software/snapshot-service/).

The host uses one hardware thread per core. Its maintenance policy is
`TERMINATE`, with automatic restart disabled, so a host event fails the run
instead of live-migrating measurements. It is deleted after results are copied
and after failures; a four-hour maximum lifetime also deletes it if local cleanup
is interrupted. Separate instances can use different physical hosts, so this
pins the advertised CPU model, not a particular physical socket. The full
instance, image, and guest `lscpu` metadata is retained with the results.

## Prerequisites

The Google Cloud CLI must be authenticated to a project with Compute Engine enabled
and permission to create and delete instances and disks. The configured VPC and
subnet must allow direct SSH or SSH through IAP. The runner does not modify firewall rules.

Create the ignored local configuration:

```sh
cp .env.gcp.example .env.gcp
```

Set `GCP_PROJECT` and adjust the network and subnet as needed. Set
`GCP_USE_IAP=1` for IAP or `GCP_NETWORK_TAGS` when the SSH firewall rule
requires instance tags. Official runs reject changes to the zone, machine type,
image project, or concrete image name.

For exploratory runs on another zone, machine, or concrete Ubuntu image, set
`GCP_BENCHMARK_PROFILE=authoring`. Such results record the selected and observed
host metadata but are not official benchmark results. Rolling image families are
not accepted in either profile; changing the pinned image should be an explicit,
reviewed repository change.

The `--min-cpu-platform` setting is intentionally not used as an exact-model
pin: it specifies a minimum CPU generation. The C3 machine series selects the
advertised CPU SKU, while the post-creation assertions enforce the benchmark
contract. See Google's [CPU platform table](https://cloud.google.com/compute/docs/cpu-platforms)
and [minimum CPU platform documentation](https://cloud.google.com/compute/docs/instances/specify-min-cpu-platform).

## Run

Commit the code to measure and use a clean worktree:

```sh
make gcp-benchmark
```

Results are stored below `benchmarks/out/gcp/` with the raw comparison JSON,
source revision, concrete OS image and its description, instance description,
guest `lscpu` data, requested host contract, pinned host-package record, and
host/container facts.

## Long-horizon run

Run the six representative Rust application workloads configured in
`long_cases.json` at 10x and 100x with VM4, VM5, and the native reference only:

```sh
make gcp-benchmark-long
```

The standard run is unchanged. This command prints native-relative geometric
means by horizon and stores raw data in `comparison-long.json`. The instruction
limit remains 100 million by default, with a 1 billion maximum.
