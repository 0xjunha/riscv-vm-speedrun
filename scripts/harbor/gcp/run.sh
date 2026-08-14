#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository=$(CDPATH= cd -- "$script_dir/../../.." && pwd)
task=harbor_tasks/riscv-vm-speedrun
startup_script=$script_dir/startup.sh
env_file=$repository/.env.gcp.harbor
agent=codex
timeout_multiplier=
keep=false
dry_run=false
starting_vm=
source_archive=
source_revision=
auth_file=
run_namespace=
upload_results=false

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
        --auth-file) [ "$#" -ge 2 ] || usage; auth_file=$2; shift 2 ;;
        --run-namespace) [ "$#" -ge 2 ] || usage; run_namespace=$2; shift 2 ;;
        --source-archive) [ "$#" -ge 2 ] || usage; source_archive=$2; shift 2 ;;
        --source-revision) [ "$#" -ge 2 ] || usage; source_revision=$2; shift 2 ;;
        --timeout-multiplier) [ "$#" -ge 2 ] || usage; timeout_multiplier=$2; shift 2 ;;
        --upload-results) upload_results=true; shift ;;
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
[ -z "$auth_file" ] || CODEX_AUTH_JSON=$auth_file

: "${GCP_PROJECT:?set GCP_PROJECT in $env_file}"
: "${GCP_ZONE:=asia-northeast3-a}"
: "${GCP_NETWORK:=default}"
: "${GCP_SUBNET:=default}"
: "${GCP_NETWORK_TAGS:=}"
: "${GCP_USE_IAP:=0}"
: "${GCP_TRAJECTORY_BUCKET:=}"
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
: "${CODEX_ALLOWED_HOSTS:=snapshot.debian.org raw.githubusercontent.com nodejs.org registry.npmjs.org chatgpt.com auth.openai.com api.openai.com}"
: "${AGENT_KWARG:=reasoning_effort=xhigh}"
: "${EXPECTED_CPUS:=4}"
: "${STARTING_VM:=vm0}"
[ -n "$starting_vm" ] || starting_vm=$STARTING_VM

for value in $agent $models $AGENT_KWARG; do
    case "$value" in
        *[!A-Za-z0-9._/=-]*) echo "agent, model, and agent kwarg values must be shell-safe" >&2; exit 2 ;;
    esac
done
for host in $CODEX_ALLOWED_HOSTS; do
    case "$host" in
        *[!A-Za-z0-9.*:/-]*) echo "Codex allowed hosts must be hostnames or IP addresses/CIDRs" >&2; exit 2 ;;
    esac
done
case "$GCP_USE_IAP" in 0|1) ;; *) echo "GCP_USE_IAP must be 0 or 1" >&2; exit 2 ;; esac
case "$GCP_TRAJECTORY_BUCKET" in
    '') ;;
    *[!a-z0-9._-]*) echo "GCP_TRAJECTORY_BUCKET must be a bucket name without gs://" >&2; exit 2 ;;
esac
[ "$upload_results" = false ] || [ -n "$GCP_TRAJECTORY_BUCKET" ] || {
    echo "--upload-results requires GCP_TRAJECTORY_BUCKET" >&2
    exit 2
}
case "$starting_vm" in vm0|vm1|vm2|vm3|vm4|vm5) ;; *) echo "starting VM must be vm0 through vm5" >&2; exit 2 ;; esac
case "$timeout_multiplier" in
    '') ;;
    .|*.*.*|*[!0-9.]*) echo "invalid timeout multiplier" >&2; exit 2 ;;
esac
case "$run_namespace" in
    *[!A-Za-z0-9-]*) echo "run namespace must contain only letters, digits, and hyphens" >&2; exit 2 ;;
esac
if [ -n "$source_archive" ] || [ -n "$source_revision" ]; then
    [ -n "$source_archive" ] && [ -n "$source_revision" ] || {
        echo "--source-archive and --source-revision must be used together" >&2
        exit 2
    }
    [ -f "$source_archive" ] || { echo "missing source archive: $source_archive" >&2; exit 2; }
    case "$source_revision" in
        *[!0-9a-f]*) echo "source revision must be a lowercase Git SHA" >&2; exit 2 ;;
    esac
    [ "${#source_revision}" -eq 40 ] || {
        echo "source revision must be a 40-character Git SHA" >&2
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
[ "$run_duration_seconds" -gt 0 ] || { echo "GCP_MAX_RUN_DURATION must be positive" >&2; exit 2; }
[ "$agent" != codex ] || [ -n "$models" ] || usage
[ "$agent" != codex ] || [ -f "$CODEX_AUTH_JSON" ] || {
    echo "missing Codex credentials: $CODEX_AUTH_JSON" >&2
    exit 2
}
required_commands="curl gcloud python3 ssh-keygen tar"
[ -n "$source_archive" ] || required_commands="$required_commands git"
for command in $required_commands; do
    command -v "$command" >/dev/null || { echo "missing command: $command" >&2; exit 2; }
done

agent_timeout_seconds=
if [ "$agent" = codex ]; then
    agent_timeout_multiplier=${timeout_multiplier:-1}
    agent_timeout_seconds=$(python3 -c 'import sys, tomllib; print(tomllib.load(open(sys.argv[1], "rb"))["agent"]["timeout_sec"] * float(sys.argv[2]))' \
        "$repository/$task/task.toml" "$agent_timeout_multiplier")
fi

# Package exactly the submitted or locally committed revision for reproducible
# remote execution.
cd "$repository"
if [ -n "$source_archive" ]; then
    revision=$source_revision
else
    [ -z "$(git status --porcelain)" ] || {
        echo "worktree is dirty; commit or stash changes before running on GCP" >&2
        exit 2
    }
    revision=$(git rev-parse HEAD)
fi
stamp=$(date -u +%Y%m%d-%H%M%S)
[ -z "$run_namespace" ] || stamp=$stamp-$(printf '%s' "$run_namespace" | tr 'A-Z' 'a-z' | cut -c1-12)
result_root=$repository/jobs/gcp/$stamp

if [ "$dry_run" = true ]; then
    printf '%-18s %s\n' \
        revision "$revision" agent "$agent" models "${models:-<none>}" \
        starting_vm "$starting_vm" \
        agent_budget "${agent_timeout_seconds:-<not available>}" \
        machine "$GCP_MACHINE_TYPE (threads-per-core=$GCP_THREADS_PER_CORE)" \
        zone "$GCP_ZONE" trajectory_bucket "${GCP_TRAJECTORY_BUCKET:-<disabled>}" \
        results "$result_root"
    exit 0
fi

temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/rv32im-harbor.XXXXXX")
archive=$temporary_dir/source.tar.gz
mkdir -p "$result_root"
printf '%s\n' "$revision" >"$result_root/source-revision.txt"
if [ -n "$source_archive" ]; then
    cp "$source_archive" "$archive"
else
    git archive --format=tar.gz --output="$archive" HEAD
fi

# Keep SSH and SCP behavior consistent for direct and IAP connections.
if [ ! -f "$HOME/.ssh/google_compute_engine" ]; then
    mkdir -p "$HOME/.ssh"
    chmod 0700 "$HOME/.ssh"
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

upload() {
    source=$1
    object=$2
    token=$(gcloud auth print-access-token) || return 1
    curl -fsS -X POST \
        -H "Authorization: Bearer $token" \
        -H 'Content-Type: application/gzip' \
        --data-binary "@$source" \
        "https://storage.googleapis.com/upload/storage/v1/b/$GCP_TRAJECTORY_BUCKET/o?uploadType=media&ifGenerationMatch=0&name=$object" \
        >/dev/null
}

log() {
    printf '[%s] %s\n' "$1" "$2"
}

slug() {
    printf '%s' "$1" | tr 'A-Z' 'a-z' | sed 's/[^a-z0-9.]\{1,\}/-/g; s/^-//; s/-$//'
}

# Run one isolated Harbor trial on its own VM.
run_one() {
    index=$1
    model=$2
    model_slug=$(slug "${model:-default}")
    effort=${AGENT_KWARG#reasoning_effort=}
    [ -n "$model" ] || effort=default
    tag=$(printf '%02d-%s-%s' "$index" "$starting_vm" "$(slug "${model:-$agent}")")
    run_id=$(printf '%s-%02d-%s-%s-%s-%s' "$stamp" "$index" "$starting_vm" "$(slug "$agent")" "$model_slug" "$(slug "$effort")")
    instance_suffix=$(printf '%s-%02d' "$stamp" "$index")
    instance_prefix=$(printf '%s' "$GCP_INSTANCE_PREFIX" | cut -c1-$((62 - ${#instance_suffix})))
    instance=$instance_prefix-$instance_suffix
    destination=$result_root/$tag
    runner=$temporary_dir/$tag-run.sh
    mkdir -p "$destination"
    printf '%s\n' "$revision" >"$destination/source-revision.txt"

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
        echo 'export HOME=/root PATH=/usr/local/bin:$PATH PYTHONPATH=/opt/harbor-run/source'
        printf 'export STARTING_VM=%s\n' "$starting_vm"
        echo 'cd /opt/harbor-run/source'
        harbor_agent=$agent
        if [ "$agent" = codex ]; then
            harbor_agent=scripts.harbor.codex_budget:BudgetCodex
        fi
        printf 'harbor run -p %s -a %s' "$task" "$harbor_agent"
        if [ -n "$model" ]; then
            printf ' -m %s --ak %s' "$model" "$AGENT_KWARG"
        fi
        if [ "$agent" = codex ]; then
            printf ' --ak budget_seconds=%s' "$agent_timeout_seconds"
            printf ' --ae CODEX_AUTH_JSON_PATH=/root/.codex/auth.json'
            for host in $CODEX_ALLOWED_HOSTS; do
                printf ' --allow-environment-host %s' "$host"
            done
        fi
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

    # Download the Harbor job, record its terminal status, and durably upload
    # asynchronous results before deciding whether the VM can be deleted.
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

    trial_status=0
    validation_status=passed
    if ! python3 - "$destination/jobs" <<'PY'
import json
import pathlib
import sys

results = list(pathlib.Path(sys.argv[1]).glob("*/result.json"))
if not results:
    raise SystemExit(1)
for path in results:
    if (json.loads(path.read_text()).get("stats") or {}).get("n_errored_trials", 0):
        raise SystemExit(1)
PY
    then
        log "$tag" "FAILED: Harbor reported an incomplete or errored trial"
        validation_status=failed
        trial_status=1
    fi
    if [ "$exit_code" -ne 0 ]; then
        log "$tag" "FAILED: Harbor exited with $exit_code"
        trial_status=1
    fi
    if [ "$trial_status" -eq 0 ]; then
        terminal_status=success
    else
        terminal_status=failed
    fi
    {
        printf 'status=%s\n' "$terminal_status"
        printf 'validation=%s\n' "$validation_status"
        printf 'harbor_exit_code=%s\n' "$exit_code"
    } >"$destination/controller-status.txt"

    upload_status=0
    should_upload=false
    if [ -n "$GCP_TRAJECTORY_BUCKET" ]; then
        if [ "$upload_results" = true ]; then
            should_upload=true
        elif [ "$trial_status" -eq 0 ] && [ "$agent" != nop ] && [ "$agent" != oracle ]; then
            should_upload=true
        fi
    fi
    if [ "$should_upload" = true ]; then
        trajectory_archive=$temporary_dir/$run_id.tgz
        log "$tag" "uploading $run_id.tgz"
        if tar -czf "$trajectory_archive" -C "$destination" . && \
            upload "$trajectory_archive" "$run_id.tgz"; then
            log "$tag" "uploaded results"
        else
            log "$tag" "FAILED: could not upload results"
            upload_status=1
        fi
    fi

    if [ "$keep" = true ]; then
        log "$tag" "done; kept $instance"
    elif [ "$upload_status" -ne 0 ]; then
        log "$tag" "kept $instance for result recovery after upload failure"
    elif [ "$trial_status" -ne 0 ]; then
        log "$tag" "kept failed $instance for inspection"
    else
        gcloud compute instances delete "$instance" --project="$GCP_PROJECT" --zone="$GCP_ZONE" --quiet >/dev/null
        log "$tag" "done; deleted $instance"
    fi
    [ "$upload_status" -eq 0 ] || return 1
    return "$trial_status"
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
