from __future__ import annotations

import io
import json
import os
import subprocess
import tarfile
from pathlib import Path

import pytest
from conftest import ROOT

SUBMIT = ROOT / "scripts/harbor/gcp/submit.sh"
CONTROLLER = ROOT / "scripts/harbor/gcp/cloud-controller.sh"
CLOUD_BUILD = ROOT / "scripts/harbor/gcp/cloudbuild.yaml"
RUN = ROOT / "scripts/harbor/gcp/run.sh"


def source_fixture(path: Path) -> None:
    content = b"committed source\n"
    member = tarfile.TarInfo("tracked.txt")
    member.size = len(content)
    with tarfile.open(path, "w:gz") as archive:
        archive.addfile(member, io.BytesIO(content))


def result_fixture(path: Path, *, errored: bool) -> None:
    files = {
        "jobs/job/result.json": json.dumps(
            {"stats": {"n_errored_trials": int(errored)}}
        ).encode(),
        "harbor.log": b"finished\n",
        "exit-code": b"0\n",
        "run.sh": b"#!/bin/sh\n",
        "host-provenance.txt": b"test host\n",
    }
    with tarfile.open(path, "w:gz") as archive:
        for name, content in files.items():
            member = tarfile.TarInfo(name)
            member.size = len(content)
            archive.addfile(member, io.BytesIO(content))


def fake_environment(tmp_path: Path) -> tuple[dict[str, str], Path, Path]:
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    fixture = tmp_path / "source.tar.gz"
    source_fixture(fixture)
    calls = tmp_path / "gcloud-calls"
    captured = tmp_path / "cloud-build-source.tar.gz"

    git = fake_bin / "git"
    git.write_text(
        """#!/bin/sh
set -eu
case "$1" in
    status) ;;
    rev-parse) echo 0123456789abcdef0123456789abcdef01234567 ;;
    archive)
        output=
        for argument in "$@"; do
            case "$argument" in --output=*) output=${argument#--output=} ;; esac
        done
        cp "$SOURCE_FIXTURE" "$output"
        ;;
    *) exit 2 ;;
esac
"""
    )
    git.chmod(0o755)

    gcloud = fake_bin / "gcloud"
    gcloud.write_text(
        """#!/bin/sh
set -eu
printf '%s\n' "$*" >>"$GCLOUD_CALLS"
case "$1 $2 $3" in
    'secrets versions add')
        echo projects/test-project/secrets/harbor-codex-auth/versions/7
        ;;
    'secrets versions describe') echo 2026-08-15T00:00:00Z ;;
    'secrets versions disable') ;;
    'secrets versions enable') ;;
    'secrets versions access') cat "$AUTH_FIXTURE" ;;
    'secrets versions destroy') ;;
    'builds submit '*)
        [ "$FAIL_BUILD_SUBMIT" = false ] || exit 1
        cp "$3" "$CAPTURED_CONTEXT"
        echo build-123
        ;;
    *) exit 2 ;;
esac
"""
    )
    gcloud.chmod(0o755)

    auth = tmp_path / "auth.json"
    auth.write_text('{"token":"private"}\n')
    env_file = tmp_path / "harbor.env"
    env_file.write_text(
        f"""GCP_PROJECT=test-project
GCP_TRAJECTORY_BUCKET=trajectory-bucket
GCP_CLOUD_BUILD_REGION=global
GCP_HARBOR_CONTROLLER_SERVICE_ACCOUNT=harbor-controller@test-project.iam.gserviceaccount.com
GCP_CODEX_AUTH_SECRET=harbor-codex-auth
CODEX_AUTH_JSON={auth}
"""
    )
    environment = os.environ | {
        "PATH": f"{fake_bin}:{os.environ['PATH']}",
        "SOURCE_FIXTURE": str(fixture),
        "GCLOUD_CALLS": str(calls),
        "CAPTURED_CONTEXT": str(captured),
        "AUTH_FIXTURE": str(auth),
        "FAIL_BUILD_SUBMIT": "false",
    }
    return environment, env_file, captured


def run_submit(
    environment: dict[str, str], env_file: Path, *arguments: str
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(SUBMIT), "--env-file", str(env_file), *arguments],
        env=environment,
        text=True,
        capture_output=True,
        check=False,
    )


def test_submit_and_controller_handoff(tmp_path: Path) -> None:
    environment, env_file, captured = fake_environment(tmp_path)

    result = run_submit(
        environment,
        env_file,
        "--starting-vm",
        "vm2",
        "openai/model-a",
        "openai/model-b",
    )

    assert result.returncode == 0, result.stderr
    assert "submitted Harbor controller: build-123" in result.stdout
    calls = (tmp_path / "gcloud-calls").read_text()
    assert "builds submit" in calls
    assert "--service-account=projects/test-project/serviceAccounts/" in calls
    assert "harbor-controller@test-project.iam.gserviceaccount.com" in calls
    assert "--async" in calls
    assert "_CODEX_AUTH_SECRET_VERSION=7" in calls
    assert "_STARTING_VM" not in calls
    assert "_MODELS" not in calls

    context = tmp_path / "context"
    context.mkdir()
    with tarfile.open(captured) as archive:
        archive.extractall(context, filter="data")
    assert (context / ".env.gcp.harbor").is_file()
    invocation = (context / ".harbor-invocation").read_text().splitlines()
    assert invocation[:4] == ["--agent", "codex", "--starting-vm", "vm2"]
    assert invocation[4:] == ["--", "openai/model-a", "openai/model-b"]
    assert (context / ".harbor-source-revision").read_text() == (
        "0123456789abcdef0123456789abcdef01234567\n"
    )
    with tarfile.open(context / ".harbor-source.tar.gz") as archive:
        assert archive.extractfile("tracked.txt").read() == b"committed source\n"
    assert not any(path.name == "auth.json" for path in context.rglob("*"))

    run_arguments = tmp_path / "run-arguments"
    captured_auth = tmp_path / "captured-auth.json"
    fake_run = tmp_path / "run.sh"
    fake_run.write_text(
        "#!/bin/sh\n"
        'printf \'%s\\n\' "$@" >"$RUN_ARGUMENTS"\n'
        'while [ "$#" -gt 0 ]; do\n'
        '  [ "$1" != --auth-file ] || { cp "$2" "$CAPTURED_AUTH"; break; }\n'
        "  shift\n"
        "done\n"
    )
    fake_run.chmod(0o755)
    controller_environment = environment | {
        "GCP_PROJECT": "test-project",
        "HARBOR_WORKSPACE": str(context),
        "HARBOR_RUN_SCRIPT": str(fake_run),
        "HARBOR_RUN_NAMESPACE": "build-123",
        "HARBOR_CODEX_AUTH_SECRET": "harbor-codex-auth",
        "HARBOR_CODEX_AUTH_SECRET_VERSION": "7",
        "RUN_ARGUMENTS": str(run_arguments),
        "CAPTURED_AUTH": str(captured_auth),
    }

    controller_result = subprocess.run(
        [str(CONTROLLER)],
        env=controller_environment,
        text=True,
        capture_output=True,
        check=False,
    )

    assert controller_result.returncode == 0, controller_result.stderr
    arguments = run_arguments.read_text().splitlines()
    assert "--upload-results" in arguments
    assert ["--run-namespace", "build-123"] == arguments[
        arguments.index("--run-namespace") :
    ][:2]
    assert arguments[-7:] == [
        "--agent",
        "codex",
        "--starting-vm",
        "vm2",
        "--",
        "openai/model-a",
        "openai/model-b",
    ]
    assert captured_auth.read_text() == '{"token":"private"}\n'
    secret_actions = [
        call.split()[2]
        for call in (tmp_path / "gcloud-calls").read_text().splitlines()
        if call.startswith("secrets versions")
    ]
    assert secret_actions == [
        "add",
        "destroy",
        "describe",
        "enable",
        "access",
        "disable",
        "destroy",
    ]


def test_failed_build_submission_schedules_and_rolls_back_auth_version(
    tmp_path: Path,
) -> None:
    environment, env_file, _ = fake_environment(tmp_path)
    environment["FAIL_BUILD_SUBMIT"] = "true"

    result = run_submit(environment, env_file, "openai/gpt-5.6-sol")

    assert result.returncode != 0
    calls = (tmp_path / "gcloud-calls").read_text().splitlines()
    add_index = next(i for i, call in enumerate(calls) if "versions add" in call)
    destroy_indices = [
        i for i, call in enumerate(calls) if "secrets versions destroy 7" in call
    ]
    submit_index = next(i for i, call in enumerate(calls) if "builds submit" in call)
    assert add_index < destroy_indices[0] < submit_index < destroy_indices[-1]


def test_submit_rejects_worker_lifetime_without_collection_margin(
    tmp_path: Path,
) -> None:
    environment, env_file, _ = fake_environment(tmp_path)
    with env_file.open("a") as config:
        config.write("GCP_MAX_RUN_DURATION=24h\n")

    result = run_submit(environment, env_file, "--dry-run", "openai/model")

    assert result.returncode == 2
    assert not (tmp_path / "gcloud-calls").exists()


def test_cloud_build_uses_managed_default_pool() -> None:
    config = CLOUD_BUILD.read_text()

    assert "google-cloud-cli:578.0.0-slim" in config
    assert "entrypoint: /workspace/scripts/harbor/gcp/cloud-controller.sh" in config
    assert "timeout: 86400s" in config
    assert "logging: CLOUD_LOGGING_ONLY" in config
    assert "workerPool" not in config
    required_environment = {
        "GCP_PROJECT=$PROJECT_ID",
        "HARBOR_RUN_NAMESPACE=$BUILD_ID",
        "HARBOR_CODEX_AUTH_SECRET=${_CODEX_AUTH_SECRET}",
        "HARBOR_CODEX_AUTH_SECRET_VERSION=${_CODEX_AUTH_SECRET_VERSION}",
    }
    assert all(setting in config for setting in required_environment)


def run_lifecycle(
    tmp_path: Path,
    *,
    errored: bool = False,
    fail_upload: bool = False,
) -> tuple[subprocess.CompletedProcess[str], list[str], Path]:
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    calls = tmp_path / "gcloud-calls"
    source = tmp_path / "source.tar.gz"
    results = tmp_path / "results.tar.gz"
    uploaded = tmp_path / "uploaded.tar.gz"
    source_fixture(source)
    result_fixture(results, errored=errored)
    gcloud = fake_bin / "gcloud"
    gcloud.write_text(
        """#!/bin/sh
set -eu
printf '%s\n' "$*" >>"$GCLOUD_CALLS"
case "$1 $2" in
    'compute instances')
        case "$3" in
            create) ;;
            describe)
                case "$*" in *--format=json*) echo '{}' ;; *) echo RUNNING ;; esac
                ;;
            delete) ;;
            *) exit 2 ;;
        esac
        ;;
    'compute ssh')
        command=
        for argument in "$@"; do
            case "$argument" in --command=*) command=${argument#--command=} ;; esac
        done
        case "$command" in
            nproc) echo 4 ;;
            'cat /opt/harbor-run/exit-code 2>/dev/null') echo "$HARBOR_EXIT_CODE" ;;
            *) ;;
        esac
        ;;
    'compute scp')
        case "$*" in
            *:/tmp/harbor-results.tgz*)
                destination=
                for destination in "$@"; do :; done
                cp "$RESULT_FIXTURE" "$destination"
                ;;
        esac
        ;;
    'auth print-access-token') echo test-token ;;
    *) exit 2 ;;
esac
"""
    )
    gcloud.chmod(0o755)
    curl = fake_bin / "curl"
    curl.write_text(
        """#!/bin/sh
set -eu
printf 'curl upload\n' >>"$GCLOUD_CALLS"
[ "$FAIL_UPLOAD" = false ] || exit 1
for argument in "$@"; do
    case "$argument" in @*) cp "${argument#@}" "$UPLOADED_ARCHIVE" ;; esac
done
"""
    )
    curl.chmod(0o755)
    env_file = tmp_path / "harbor.env"
    env_file.write_text(
        """GCP_PROJECT=test-project
GCP_ZONE=asia-northeast3-a
GCP_USE_IAP=0
GCP_TRAJECTORY_BUCKET=trajectory-bucket
GCP_MAX_RUN_DURATION=1h
EXPECTED_CPUS=4
"""
    )
    home = tmp_path / "home"
    home.mkdir()
    environment = os.environ | {
        "PATH": f"{fake_bin}:{os.environ['PATH']}",
        "HOME": str(home),
        "GCLOUD_CALLS": str(calls),
        "RESULT_FIXTURE": str(results),
        "UPLOADED_ARCHIVE": str(uploaded),
        "FAIL_UPLOAD": str(fail_upload).lower(),
        "HARBOR_EXIT_CODE": "0",
    }
    result = subprocess.run(
        [
            str(RUN),
            "--agent",
            "oracle",
            "--env-file",
            str(env_file),
            "--source-archive",
            str(source),
            "--source-revision",
            "0123456789abcdef0123456789abcdef01234567",
            "--run-namespace",
            "build-123",
            "--upload-results",
        ],
        env=environment,
        text=True,
        capture_output=True,
        check=False,
    )
    return result, calls.read_text().splitlines(), uploaded


@pytest.mark.parametrize(
    ("errored", "fail_upload", "uploaded_result", "deleted"),
    [
        (False, False, True, True),
        (False, True, False, False),
        (True, False, True, False),
    ],
    ids=["success", "upload-failure", "failed-trial"],
)
def test_async_result_lifecycle(
    tmp_path: Path,
    errored: bool,
    fail_upload: bool,
    uploaded_result: bool,
    deleted: bool,
) -> None:
    result, calls, uploaded = run_lifecycle(
        tmp_path, errored=errored, fail_upload=fail_upload
    )

    assert result.returncode == (0 if deleted else 1)
    assert uploaded.is_file() is uploaded_result
    delete_calls = [
        call for call in calls if call.startswith("compute instances delete")
    ]
    assert bool(delete_calls) is deleted
    create = next(call for call in calls if "instances create" in call)
    assert "-build-123-01" in create
    assert all(
        flag in create
        for flag in (
            "--no-service-account",
            "--no-scopes",
            "--max-run-duration=1h",
            "--instance-termination-action=DELETE",
        )
    )
    if deleted:
        upload_index = next(
            i for i, call in enumerate(calls) if call == "curl upload"
        )
        assert upload_index < calls.index(delete_calls[0])
    if uploaded_result:
        with tarfile.open(uploaded) as archive:
            status = next(
                member
                for member in archive.getmembers()
                if member.name in {"controller-status.txt", "./controller-status.txt"}
            )
            expected = b"status=failed\n" if errored else b"status=success\n"
            assert archive.extractfile(status).read().startswith(expected)
