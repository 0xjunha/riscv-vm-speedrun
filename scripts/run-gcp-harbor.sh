#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
task=harbor_tasks/riscv-vm-speedrun
startup_script=$repository/scripts/gcp-harbor-startup.sh
env_file=$repository/.env.gcp.harbor
agent=codex
timeout_multiplier=
keep=false
dry_run=false
starting_vm=

# Load run options and the workflow-specific GCP configuration.
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
set -a
. "$env_file"
set +a

: "${GCP_PROJECT:?set GCP_PROJECT in $env_file}"
: "${GCP_ZONE:=asia-northeast3-a}"
: "${GCP_NETWORK:=default}"
: "${GCP_SUBNET:=default}"
: "${GCP_NETWORK_TAGS:=}"
: "${GCP_USE_IAP:=0}"
: "${GCP_MACHINE_TYPE:=c3-highcpu-8}"
: "${GCP_THREADS_PER_CORE:=1}"
: "${GCP_IMAGE_PROJECT:=ubuntu-os-cloud}"
: "${GCP_IMAGE:=ubuntu-2404-noble-amd64-v20260723}"
: "${GCP_BOOT_DISK_SIZE:=50GB}"
: "${GCP_BOOT_DISK_TYPE:=hyperdisk-balanced}"
: "${GCP_INSTANCE_PREFIX:=rv32im-harbor}"
: "${GCP_MAX_RUN_DURATION:=12h}"
: "${HARBOR_VERSION:=0.20.0}"
: "${UV_VERSION:=0.9.9}"
: "${CODEX_AUTH_JSON:=$HOME/.codex/auth.json}"
: "${AGENT_KWARG:=reasoning_effort=xhigh}"
: "${EXPECTED_CPUS:=4}"
: "${STARTING_VM:=vm0}"
[ -n "$starting_vm" ] || starting_vm=$STARTING_VM

for value in $agent $models $AGENT_KWARG; do
    case "$value" in
        *[!A-Za-z0-9._/=-]*) echo "agent, model, and agent kwarg values must be shell-safe" >&2; exit 2 ;;
    esac
done
case "$GCP_USE_IAP" in 0|1) ;; *) echo "GCP_USE_IAP must be 0 or 1" >&2; exit 2 ;; esac
case "$starting_vm" in vm0|vm1|vm2|vm3|vm4|vm5) ;; *) echo "starting VM must be vm0 through vm5" >&2; exit 2 ;; esac
case "$timeout_multiplier" in
    '') ;;
    .|*.*.*|*[!0-9.]*) echo "invalid timeout multiplier" >&2; exit 2 ;;
esac
case "$GCP_MAX_RUN_DURATION" in
    *s) run_multiplier=1; run_value=${GCP_MAX_RUN_DURATION%s} ;;
    *m) run_multiplier=60; run_value=${GCP_MAX_RUN_DURATION%m} ;;
    *h) run_multiplier=3600; run_value=${GCP_MAX_RUN_DURATION%h} ;;
    *d) run_multiplier=86400; run_value=${GCP_MAX_RUN_DURATION%d} ;;
    *) echo "GCP_MAX_RUN_DURATION must end in s, m, h, or d" >&2; exit 2 ;;
esac
case "$run_value" in ''|*[!0-9]*) echo "invalid GCP_MAX_RUN_DURATION" >&2; exit 2 ;; esac
run_duration_seconds=$((run_value * run_multiplier))
[ "$run_duration_seconds" -gt 0 ] || { echo "GCP_MAX_RUN_DURATION must be positive" >&2; exit 2; }
[ "$agent" != codex ] || [ -n "$models" ] || usage
[ "$agent" != codex ] || [ -f "$CODEX_AUTH_JSON" ] || {
    echo "missing Codex credentials: $CODEX_AUTH_JSON" >&2
    exit 2
}
for command in gcloud git python3 ssh-keygen tar; do
    command -v "$command" >/dev/null || { echo "missing command: $command" >&2; exit 2; }
done

# Package exactly the committed revision for reproducible remote execution.
cd "$repository"
[ -z "$(git status --porcelain)" ] || {
    echo "worktree is dirty; commit or stash changes before running on GCP" >&2
    exit 2
}

revision=$(git rev-parse HEAD)
stamp=$(date -u +%Y%m%d-%H%M%S)
result_root=$repository/jobs/gcp/$stamp

if [ "$dry_run" = true ]; then
    printf '%-18s %s\n' \
        revision "$revision" agent "$agent" models "${models:-<none>}" \
        starting_vm "$starting_vm" \
        machine "$GCP_MACHINE_TYPE (threads-per-core=$GCP_THREADS_PER_CORE)" \
        zone "$GCP_ZONE" results "$result_root"
    exit 0
fi

temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/rv32im-harbor.XXXXXX")
archive=$temporary_dir/source.tar.gz
mkdir -p "$result_root"
printf '%s\n' "$revision" >"$result_root/source-revision.txt"
git archive --format=tar.gz --output="$archive" HEAD

# Keep SSH and SCP behavior consistent for direct and IAP connections.
if [ ! -f "$HOME/.ssh/google_compute_engine" ]; then
    ssh-keygen -q -t rsa -b 2048 -N '' -f "$HOME/.ssh/google_compute_engine"
fi

remote() {
    instance=$1
    shift
    set -- gcloud compute ssh "$instance" --project="$GCP_PROJECT" --zone="$GCP_ZONE" \
        --ssh-flag=-oConnectTimeout=15 --command="$*" --quiet
    [ "$GCP_USE_IAP" = 0 ] || set -- "$@" --tunnel-through-iap
    "$@"
}

copy() {
    set -- gcloud compute scp --project="$GCP_PROJECT" --zone="$GCP_ZONE" --quiet "$@"
    [ "$GCP_USE_IAP" = 0 ] || set -- "$@" --tunnel-through-iap
    "$@"
}

log() {
    printf '[%s] %s\n' "$1" "$2"
}

# Run one isolated Harbor trial on its own VM.
run_one() {
    index=$1
    model=$2
    slug=$(printf '%s' "${model:-$agent}" | tr 'A-Z' 'a-z' | sed 's/[^a-z0-9]\{1,\}/-/g; s/^-//; s/-$//')
    tag=$(printf '%02d-%s-%s' "$index" "$starting_vm" "$slug")
    instance=$(printf '%s-%s-%02d' "$GCP_INSTANCE_PREFIX" "$stamp" "$index" | cut -c1-63 | sed 's/-$//')
    destination=$result_root/$tag
    runner=$temporary_dir/$tag-run.sh
    mkdir -p "$destination"

    fail() {
        log "$tag" "FAILED: $1"
        if [ "${2:-}" = gone ]; then
            log "$tag" "$instance no longer exists"
        else
            log "$tag" "kept $instance for inspection"
            log "$tag" "delete with: gcloud compute instances delete $instance --project=$GCP_PROJECT --zone=$GCP_ZONE"
        fi
        return 1
    }

    check_instance() {
        phase=$1
        if ! instance_status=$(gcloud compute instances describe "$instance" \
            --project="$GCP_PROJECT" --zone="$GCP_ZONE" --format='value(status)' 2>/dev/null); then
            if instance_name=$(gcloud compute instances list --project="$GCP_PROJECT" \
                --zones="$GCP_ZONE" --filter="name=$instance" --format='value(name)' 2>/dev/null); then
                [ "$instance_name" = "$instance" ] || { fail "instance disappeared $phase" gone; return 1; }
            fi
            log "$tag" "could not query instance status; retrying"
            return 0
        fi
        case "$instance_status" in
            RUNNING) return 0 ;;
            '') fail "instance disappeared $phase" gone ;;
            *) fail "instance entered $instance_status $phase" ;;
        esac
    }

    # Build the command the VM executes after bootstrap.
    {
        echo '#!/bin/sh'
        echo 'set -u'
        echo 'export HOME=/root PATH=/usr/local/bin:$PATH'
        printf 'export STARTING_VM=%s\n' "$starting_vm"
        echo 'cd /opt/harbor-run/source'
        printf 'harbor run -p %s -a %s' "$task" "$agent"
        if [ -n "$model" ]; then
            printf ' -m %s --ak %s' "$model" "$AGENT_KWARG"
        fi
        [ "$agent" != codex ] || printf ' --ae CODEX_FORCE_AUTH_JSON=1'
        [ -z "$timeout_multiplier" ] || printf ' --agent-timeout-multiplier %s' "$timeout_multiplier"
        echo ' -o /opt/harbor-run/jobs --yes >>/opt/harbor-run/harbor.log 2>&1'
        echo 'status=$?'
        echo 'printf "%s\n" "$status" >/opt/harbor-run/exit-code'
        echo 'exit "$status"'
    } >"$runner"

    # Create a self-deleting VM with the pinned bootstrap script.
    log "$tag" "creating $instance"
    instance_deadline=$(($(date +%s) + run_duration_seconds + 300))
    set -- gcloud compute instances create "$instance" \
        --project="$GCP_PROJECT" --zone="$GCP_ZONE" \
        --machine-type="$GCP_MACHINE_TYPE" --threads-per-core="$GCP_THREADS_PER_CORE" \
        --provisioning-model=STANDARD --maintenance-policy=TERMINATE --no-restart-on-failure \
        --image="$GCP_IMAGE" --image-project="$GCP_IMAGE_PROJECT" \
        --boot-disk-size="$GCP_BOOT_DISK_SIZE" --boot-disk-type="$GCP_BOOT_DISK_TYPE" --boot-disk-auto-delete \
        --network-interface="network=$GCP_NETWORK,subnet=$GCP_SUBNET,nic-type=GVNIC,stack-type=IPV4_ONLY" \
        --no-service-account --no-scopes \
        --metadata-from-file="startup-script=$startup_script" \
        --metadata="harbor-version=$HARBOR_VERSION,uv-version=$UV_VERSION" \
        --labels=managed-by=rv32im-harbor \
        --max-run-duration="$GCP_MAX_RUN_DURATION" --instance-termination-action=DELETE \
        --shielded-secure-boot --shielded-vtpm --shielded-integrity-monitoring --quiet
    [ -z "$GCP_NETWORK_TAGS" ] || set -- "$@" --tags="$GCP_NETWORK_TAGS"
    "$@" || { log "$tag" "instance creation failed"; return 1; }

    gcloud compute instances describe "$instance" --project="$GCP_PROJECT" --zone="$GCP_ZONE" \
        --format=json >"$destination/instance.json" || true

    # Wait for bootstrap and verify the CPU topology used for scoring.
    log "$tag" "waiting for bootstrap"
    deadline=$(($(date +%s) + 1200))
    while ! remote "$instance" "test -f /var/tmp/rv32im-harbor-ready" >/dev/null 2>&1; do
        if remote "$instance" "test -f /var/tmp/rv32im-harbor-failed" >/dev/null 2>&1; then
            fail "startup failed"
            return 1
        fi
        check_instance "during bootstrap" || return 1
        if [ "$(date +%s)" -ge "$instance_deadline" ]; then
            fail "instance lifetime expired during bootstrap"
            return 1
        fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            fail "startup exceeded 20 minutes"
            return 1
        fi
        sleep 10
    done
    host_cpus=$(remote "$instance" nproc 2>/dev/null | tr -d '\r\n ')
    [ "$host_cpus" = "$EXPECTED_CPUS" ] || {
        fail "expected $EXPECTED_CPUS CPUs, found ${host_cpus:-unknown}"
        return 1
    }

    # Upload the committed tree and credentials, then launch Harbor.
    log "$tag" "uploading task"
    copy "$archive" "$instance:/tmp/source.tar.gz" >/dev/null
    copy "$runner" "$instance:/tmp/run.sh" >/dev/null
    if [ "$agent" = codex ]; then
        # auth.json contains reusable access tokens. Copying it to this trusted,
        # ephemeral VM is a deliberate tradeoff for unattended Codex authentication.
        copy "$CODEX_AUTH_JSON" "$instance:/tmp/auth.json" >/dev/null
    fi
    setup='sudo mkdir -p /opt/harbor-run/source /opt/harbor-run/jobs && sudo tar -xzf /tmp/source.tar.gz -C /opt/harbor-run/source && sudo install -m 0755 /tmp/run.sh /opt/harbor-run/run.sh && rm -f /tmp/source.tar.gz /tmp/run.sh'
    if [ "$agent" = codex ]; then
        setup="$setup && sudo install -D -m 0600 /tmp/auth.json /root/.codex/auth.json && rm -f /tmp/auth.json"
    fi
    remote "$instance" "$setup" >/dev/null || { fail "upload setup failed"; return 1; }

    log "$tag" "starting Harbor"
    remote "$instance" "sudo systemd-run --unit=harbor-run /opt/harbor-run/run.sh" >/dev/null || {
        fail "could not start Harbor"
        return 1
    }

    # Poll for completion while enforcing instance state and lifetime.
    ticks=0
    while :; do
        exit_code=$(remote "$instance" "cat /opt/harbor-run/exit-code 2>/dev/null" 2>/dev/null | tr -d '\r\n ')
        case "$exit_code" in ''|*[!0-9]*) ;; *) break ;; esac
        ticks=$((ticks + 1))
        if [ $((ticks % 10)) -eq 0 ]; then
            check_instance "before producing results" || return 1
            tail_line=$(remote "$instance" "sudo tail -n 1 /opt/harbor-run/harbor.log 2>/dev/null" 2>/dev/null | tr -d '\r' | tail -n 1)
            log "$tag" "running: ${tail_line:-no output}"
        fi
        [ "$(date +%s)" -lt "$instance_deadline" ] || { fail "run exceeded instance lifetime"; return 1; }
        sleep 30
    done

    # Download the Harbor job and reject incomplete or errored trials.
    log "$tag" "collecting results"
    remote "$instance" "sudo tar -czf /tmp/harbor-results.tgz -C /opt/harbor-run jobs harbor.log exit-code run.sh host-provenance.txt && sudo chmod 0644 /tmp/harbor-results.tgz" >/dev/null || {
        fail "could not archive results"
        return 1
    }
    copy "$instance:/tmp/harbor-results.tgz" "$destination/results.tgz" >/dev/null || {
        fail "could not download results"
        return 1
    }
    tar -xzf "$destination/results.tgz" -C "$destination"
    rm -f "$destination/results.tgz"

    python3 - "$destination/jobs" <<'PY' || { fail "Harbor reported an errored trial"; return 1; }
import json
import pathlib
import sys

results = list(pathlib.Path(sys.argv[1]).rglob("result.json"))
if not results:
    raise SystemExit(1)
for path in results:
    if (json.loads(path.read_text()).get("stats") or {}).get("n_errored_trials", 0):
        raise SystemExit(1)
PY
    [ "$exit_code" -eq 0 ] || { fail "Harbor exited with $exit_code"; return 1; }

    if [ "$keep" = true ]; then
        log "$tag" "done; kept $instance"
    else
        gcloud compute instances delete "$instance" --project="$GCP_PROJECT" --zone="$GCP_ZONE" --quiet >/dev/null
        log "$tag" "done; deleted $instance"
    fi
}

# Run one VM per model in parallel and aggregate failures.
[ -n "$models" ] || models=__none__
pids=
index=0
for model in $models; do
    index=$((index + 1))
    [ "$model" != __none__ ] || model=
    run_one "$index" "$model" &
    pids="$pids $!"
done

status=0
for pid in $pids; do
    wait "$pid" || status=1
done
rm -rf "$temporary_dir"

echo "results: $result_root"
find "$result_root" -name reward.json -print -exec sed -n '1,20p' {} \;
exit "$status"
