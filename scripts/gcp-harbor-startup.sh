#!/bin/sh
set -eu

# Pin the host packages that affect container execution.
run_root=/opt/harbor-run
ready_marker=/var/tmp/rv32im-harbor-ready
failed_marker=/var/tmp/rv32im-harbor-failed
ubuntu_snapshot=20260723T000000Z
docker_version=29.1.3-0ubuntu3~24.04.2
buildx_version=0.30.1-0ubuntu1~24.04.1
containerd_version=2.2.1-0ubuntu1~24.04.3
runc_version=1.3.4-0ubuntu1~24.04.1
compose_version=2.40.3+ds1-0ubuntu1~24.04.1

# Expose bootstrap failure to the controller.
mark_failure() {
    status=$?
    [ "$status" -eq 0 ] || printf '%s\n' "$status" >"$failed_marker"
}
trap mark_failure EXIT

metadata() {
    curl -fsS -H 'Metadata-Flavor: Google' \
        "http://metadata.google.internal/computeMetadata/v1/instance/attributes/$1"
}

# Validate the base image and install Docker from a pinned Ubuntu snapshot.
. /etc/os-release
[ "${VERSION_CODENAME:-}" = noble ]
[ "$(dpkg --print-architecture)" = amd64 ]

systemctl mask --now apt-daily.timer apt-daily-upgrade.timer >/dev/null

rm -f /etc/apt/sources.list.d/ubuntu.sources
{
    printf '%s\n' \
        'Types: deb' \
        'URIs: https://archive.ubuntu.com/ubuntu' \
        'Suites: noble noble-updates' \
        'Components: main universe' \
        'Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg' \
        "Snapshot: $ubuntu_snapshot" \
        '' \
        'Types: deb' \
        'URIs: https://security.ubuntu.com/ubuntu' \
        'Suites: noble-security' \
        'Components: main universe' \
        'Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg' \
        "Snapshot: $ubuntu_snapshot"
} >/etc/apt/sources.list.d/harbor.sources

export DEBIAN_FRONTEND=noninteractive
apt-get -o APT::Update::Error-Mode=any update
apt-get install -y --no-install-recommends \
    ca-certificates curl \
    "docker.io=$docker_version" \
    "docker-buildx=$buildx_version" \
    "containerd=$containerd_version" \
    "runc=$runc_version" \
    "docker-compose-v2=$compose_version"

systemctl enable --now docker
docker buildx version >/dev/null
docker compose version >/dev/null

# Install the Harbor version requested through instance metadata.
uv_version=$(metadata uv-version)
harbor_version=$(metadata harbor-version)
curl -LsSf "https://astral.sh/uv/$uv_version/install.sh" -o /tmp/uv-install.sh
env UV_INSTALL_DIR=/usr/local/bin INSTALLER_NO_MODIFY_PATH=1 sh /tmp/uv-install.sh
rm -f /tmp/uv-install.sh
env UV_TOOL_DIR=/opt/uv/tools UV_TOOL_BIN_DIR=/usr/local/bin \
    uv tool install "harbor==$harbor_version"

# Record the host environment before marking the VM ready.
mkdir -p "$run_root"
{
    printf 'ubuntu_snapshot=%s\n' "$ubuntu_snapshot"
    dpkg-query -W -f='${Package}=${Version}\n' \
        docker.io docker-buildx containerd runc docker-compose-v2
    uv --version
    harbor --version
    lscpu
} >"$run_root/host-provenance.txt"
touch "$ready_marker"
