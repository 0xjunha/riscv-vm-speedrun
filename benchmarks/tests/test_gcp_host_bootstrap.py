from __future__ import annotations

import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
STARTUP = ROOT / "benchmarks/gcp/startup.sh"
RUN_ON_VM = ROOT / "benchmarks/gcp/run-on-vm.sh"


def _assignment(source: str, name: str) -> str:
    match = re.search(rf"^{re.escape(name)}=([^\n]+)$", source, re.MULTILINE)
    assert match is not None, f"missing {name} assignment"
    return match.group(1)


def test_gcp_host_scripts_are_posix_shell_syntax() -> None:
    completed = subprocess.run(
        ["sh", "-n", str(STARTUP), str(RUN_ON_VM)],
        check=False,
        capture_output=True,
        text=True,
    )

    assert completed.returncode == 0, completed.stderr


def test_gcp_startup_has_a_fail_closed_package_contract() -> None:
    source = STARTUP.read_text(encoding="utf-8")

    assert _assignment(source, "ubuntu_snapshot") == "20260723T000000Z"
    assert _assignment(source, "docker_version") == "29.1.3-0ubuntu3~24.04.2"
    assert _assignment(source, "containerd_version") == "2.2.1-0ubuntu1~24.04.3"
    assert _assignment(source, "runc_version") == "1.3.4-0ubuntu1~24.04.1"
    assert source.count("Snapshot: $ubuntu_snapshot") == 2
    assert "Dir::Etc::sourcelist=$apt_sources_name" in source
    assert "Dir::Etc::sourceparts=$apt_source_parts_name" in source
    assert "Dir::State::lists=$apt_lists_name" in source
    assert "APT::Update::Error-Mode=any" in source
    assert '"docker.io=$docker_version"' in source
    assert '"containerd=$containerd_version"' in source
    assert '"runc=$runc_version"' in source

    ready = source.index("touch /var/tmp/rv32im-benchmark-ready")
    assert source.index("require_package_version docker.io") < ready
    assert source.index("require_package_version containerd") < ready
    assert source.index("require_package_version runc") < ready
    assert source.index("systemctl is-active --quiet docker") < ready


def test_gcp_run_records_the_host_package_contract() -> None:
    source = RUN_ON_VM.read_text(encoding="utf-8")

    assert "/var/tmp/rv32im-benchmark-host-packages.txt" in source
    assert '"$result_dir/host-packages.txt"' in source
