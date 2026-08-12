from __future__ import annotations

import hashlib
import os
import shlex
import shutil
import stat
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/benchmarks/run-gcp.sh"
ENV_EXAMPLE = ROOT / ".env.gcp.example"
RESULTS = ROOT / "benchmarks/out/gcp"
OFFICIAL_ZONE = "asia-northeast3-a"
OFFICIAL_MACHINE = "c3-highcpu-4"
OFFICIAL_IMAGE = "ubuntu-2404-noble-amd64-v20260723"
OFFICIAL_PLATFORM = "Intel Sapphire Rapids"
OFFICIAL_MODEL = "Intel(R) Xeon(R) Platinum 8481C CPU @ 2.70GHz"
OFFICIAL_TIMEOUT = "30"


def _write_executable(path: Path, contents: str) -> None:
    path.write_text(contents, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


def _install_fake_commands(tmp_path: Path) -> tuple[Path, Path]:
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    log = tmp_path / "gcloud.log"

    _write_executable(
        fake_bin / "python3",
        f"""#!/bin/sh
if [ "${{1:-}}" = benchmarks/build.py ]; then
    exit 0
fi
exec {shlex.quote(sys.executable)} "$@"
""",
    )
    _write_executable(
        fake_bin / "git",
        """#!/bin/sh
case "$1" in
    status)
        exit 0
        ;;
    rev-parse)
        printf '%s\n' 0123456789abcdef0123456789abcdef01234567
        ;;
    archive)
        archive_output=
        for argument in "$@"; do
            case "$argument" in
                --output=*) archive_output=${argument#--output=} ;;
            esac
        done
        : >"$archive_output"
        ;;
    *)
        echo "unexpected fake git invocation: $*" >&2
        exit 99
        ;;
esac
""",
    )
    _write_executable(
        fake_bin / "gcloud",
        """#!/bin/sh
printf '%s\n' "$*" >>"$FAKE_GCLOUD_LOG"

case "$1 $2 $3" in
    "compute images describe")
        printf '{"name":"%s"}\n' "$FAKE_IMAGE"
        ;;
    "compute instances describe")
        case " $* " in
            *" --format=json "*)
                printf '{"zone":"zones/%s","machineType":"machineTypes/%s","cpuPlatform":"%s","advancedMachineFeatures":{"threadsPerCore":%s},"scheduling":{"onHostMaintenance":"%s","automaticRestart":%s}}\n' \
                    "$FAKE_ZONE" "$FAKE_MACHINE_TYPE" "$FAKE_CPU_PLATFORM" \
                    "$FAKE_THREADS_PER_CORE" "$FAKE_MAINTENANCE_POLICY" \
                    "$FAKE_AUTOMATIC_RESTART"
                ;;
            *) exit 1 ;;
        esac
        ;;
    "compute instances create" | "compute instances delete")
        ;;
    "compute ssh "*)
        case "$*" in
            *"test -f /var/tmp/rv32im-benchmark-ready"*) ;;
            *"lscpu --json"*)
                printf '{"lscpu":[{"field":"Model name:","data":"%s"},{"field":"Thread(s) per core:","data":"%s"}]}\n' \
                    "$FAKE_CPU_MODEL" "$FAKE_THREADS_PER_CORE"
                ;;
            *"run-on-vm.sh"*) ;;
            *)
                echo "unexpected fake gcloud ssh invocation: $*" >&2
                exit 99
                ;;
        esac
        ;;
    "compute scp "*) ;;
    *)
        echo "unexpected fake gcloud invocation: $*" >&2
        exit 99
        ;;
esac
""",
    )
    return fake_bin, log


@pytest.fixture
def instance_prefix(tmp_path: Path):
    suffix = hashlib.sha256(str(tmp_path).encode()).hexdigest()[:8]
    prefix = f"gcp-test-{suffix}"
    yield prefix
    for result_dir in RESULTS.glob(f"{prefix}-*"):
        shutil.rmtree(result_dir)


def _environment(fake_bin: Path, log: Path, **overrides: str) -> dict[str, str]:
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("GCP_", "BENCHMARK_", "FAKE_"))
    }
    environment.update(
        {
            "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
            "FAKE_GCLOUD_LOG": str(log),
            "FAKE_ZONE": OFFICIAL_ZONE,
            "FAKE_MACHINE_TYPE": OFFICIAL_MACHINE,
            "FAKE_IMAGE": OFFICIAL_IMAGE,
            "FAKE_CPU_PLATFORM": OFFICIAL_PLATFORM,
            "FAKE_CPU_MODEL": OFFICIAL_MODEL,
            "FAKE_THREADS_PER_CORE": "1",
            "FAKE_MAINTENANCE_POLICY": "TERMINATE",
            "FAKE_AUTOMATIC_RESTART": "false",
        }
    )
    environment.update(overrides)
    return environment


def _write_config(path: Path, **values: str) -> None:
    settings = {
        "GCP_PROJECT": "test-project",
        **values,
    }
    path.write_text(
        "".join(f"{key}={value}\n" for key, value in settings.items()),
        encoding="utf-8",
    )


def _run(config: Path, environment: dict[str, str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(SCRIPT), str(config)],
        cwd=ROOT,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
    )


def test_official_profile_pins_and_validates_host(
    tmp_path: Path, instance_prefix: str
) -> None:
    fake_bin, log = _install_fake_commands(tmp_path)
    config = tmp_path / "gcp.env"
    _write_config(config, GCP_INSTANCE_PREFIX=instance_prefix)

    completed = _run(config, _environment(fake_bin, log))

    assert completed.returncode == 0, completed.stderr
    calls = log.read_text(encoding="utf-8")
    assert "compute images describe " + OFFICIAL_IMAGE in calls
    assert f"--zone={OFFICIAL_ZONE}" in calls
    assert f"--machine-type={OFFICIAL_MACHINE}" in calls
    assert f"--image={OFFICIAL_IMAGE}" in calls
    assert "--threads-per-core=1" in calls
    assert "--maintenance-policy=TERMINATE" in calls
    assert "--no-restart-on-failure" in calls
    assert f"'{OFFICIAL_TIMEOUT}' 'standard'" in calls
    assert f"BENCHMARK_TIMEOUT_SECONDS={OFFICIAL_TIMEOUT}\n" in ENV_EXAMPLE.read_text(
        encoding="utf-8"
    )
    assert calls.index("lscpu --json") < calls.index("scp ")

    result_dir = next(RESULTS.glob(f"{instance_prefix}-*"))
    host_contract = (result_dir / "host-contract.txt").read_text(encoding="utf-8")
    assert OFFICIAL_PLATFORM in host_contract
    assert "maintenance_policy=TERMINATE" in host_contract
    assert "automatic_restart=false" in host_contract
    assert OFFICIAL_MODEL in (result_dir / "host-lscpu.json").read_text()
    assert (result_dir / "source-image.json").is_file()


@pytest.mark.parametrize(
    ("setting", "value"),
    [
        ("GCP_ZONE", "us-central1-a"),
        ("GCP_MACHINE_TYPE", "c3-highcpu-8"),
        ("GCP_IMAGE_PROJECT", "custom-images"),
        ("GCP_IMAGE", "another-image"),
    ],
)
def test_official_profile_rejects_configuration_drift(
    tmp_path: Path, setting: str, value: str
) -> None:
    config = tmp_path / "gcp.env"
    _write_config(config, **{setting: value})
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("GCP_", "BENCHMARK_", "FAKE_"))
    }

    completed = _run(config, environment)

    assert completed.returncode == 2
    assert f"official GCP benchmark requires {setting}=" in completed.stderr


@pytest.mark.parametrize(
    ("environment_override", "message"),
    [
        ({"FAKE_CPU_PLATFORM": "Intel Ice Lake"}, "CPU platform"),
        ({"FAKE_CPU_MODEL": "unexpected model"}, "CPU model"),
        ({"FAKE_THREADS_PER_CORE": "2"}, "threads per core"),
        ({"FAKE_MAINTENANCE_POLICY": "MIGRATE"}, "maintenance policy"),
        ({"FAKE_AUTOMATIC_RESTART": "true"}, "automatic restart"),
    ],
)
def test_official_profile_rejects_observed_host_drift(
    tmp_path: Path,
    instance_prefix: str,
    environment_override: dict[str, str],
    message: str,
) -> None:
    fake_bin, log = _install_fake_commands(tmp_path)
    config = tmp_path / "gcp.env"
    _write_config(config, GCP_INSTANCE_PREFIX=instance_prefix)

    completed = _run(
        config,
        _environment(fake_bin, log, **environment_override),
    )

    assert completed.returncode == 1
    assert message in completed.stderr
    calls = log.read_text(encoding="utf-8")
    assert "compute scp" not in calls
    assert "run-on-vm.sh" not in calls


def test_authoring_profile_allows_explicit_host_overrides(
    tmp_path: Path, instance_prefix: str
) -> None:
    fake_bin, log = _install_fake_commands(tmp_path)
    config = tmp_path / "gcp.env"
    _write_config(
        config,
        GCP_BENCHMARK_PROFILE="authoring",
        GCP_ZONE="us-central1-a",
        GCP_MACHINE_TYPE="c3-highcpu-8",
        GCP_IMAGE_PROJECT="custom-images",
        GCP_IMAGE="custom-ubuntu-image",
        GCP_INSTANCE_PREFIX=instance_prefix,
    )
    environment = _environment(
        fake_bin,
        log,
        FAKE_ZONE="us-central1-a",
        FAKE_MACHINE_TYPE="c3-highcpu-8",
        FAKE_IMAGE="custom-ubuntu-image",
        FAKE_CPU_PLATFORM="Custom platform",
        FAKE_CPU_MODEL="Custom CPU model",
    )

    completed = _run(config, environment)

    assert completed.returncode == 0, completed.stderr
    calls = log.read_text(encoding="utf-8")
    assert "--zone=us-central1-a" in calls
    assert "--machine-type=c3-highcpu-8" in calls
    assert "--image=custom-ubuntu-image" in calls
