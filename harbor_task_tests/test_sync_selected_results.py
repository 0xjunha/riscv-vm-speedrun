from __future__ import annotations

import io
import os
import subprocess
import tarfile
from pathlib import Path

from conftest import ROOT

SCRIPT = ROOT / "scripts/harbor/gcp/sync-selected-results.sh"


def write_archive(path: Path, name: str, content: str) -> None:
    data = content.encode()
    member = tarfile.TarInfo(name)
    member.size = len(data)
    path.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(path, "w:gz") as archive:
        archive.addfile(member, io.BytesIO(data))


def test_sync_keeps_only_current_safe_extractions(tmp_path: Path) -> None:
    fake_bin = tmp_path / "bin"
    bucket = tmp_path / "bucket"
    destination = tmp_path / "selected"
    objects = tmp_path / "objects.tsv"
    fake_bin.mkdir()
    bucket.mkdir()
    gcloud = fake_bin / "gcloud"
    gcloud.write_text(
        """#!/bin/sh
set -eu
case "$1 $2 $3" in
    'storage objects list') cat "$OBJECTS" ;;
    'storage cp '*) cp "$BUCKET/${3#gs://selected-harbor-results/}" "$4" ;;
    *) exit 2 ;;
esac
"""
    )
    gcloud.chmod(0o755)
    environment = os.environ | {
        "PATH": f"{fake_bin}:{os.environ['PATH']}",
        "OBJECTS": str(objects),
        "BUCKET": str(bucket),
        "GCP_SELECTED_RESULTS_DESTINATION": str(destination),
    }

    def sync() -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(SCRIPT)], env=environment, text=True, capture_output=True, check=False
        )

    remote = bucket / "group/run.tgz"
    objects.write_text("group/run.tgz\t1\n")
    write_archive(remote, "harbor.log", "first\n")
    write_archive(destination / "group/run.tgz", "harbor.log", "legacy\n")
    assert sync().returncode == 0
    extracted = destination / "group/run"
    assert (extracted / "harbor.log").read_text() == "first\n"
    assert (extracted / ".gcs-generation").read_text() == "1\n"
    assert not list(destination.rglob("*.tgz"))

    remote.unlink()
    (extracted / "local-note").touch()
    assert sync().returncode == 0
    assert (extracted / "local-note").exists()

    objects.write_text("group/run.tgz\t2\n")
    write_archive(remote, "harbor.log", "second\n")
    assert sync().returncode == 0
    assert (extracted / "harbor.log").read_text() == "second\n"
    assert not (extracted / "local-note").exists()
    assert not list(destination.rglob("*.tgz"))

    objects.write_text("group/run.tgz\t3\n")
    write_archive(remote, "../escaped", "unsafe\n")
    assert sync().returncode != 0
    assert (extracted / "harbor.log").read_text() == "second\n"
    assert not (tmp_path / "escaped").exists()
