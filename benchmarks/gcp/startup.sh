#!/bin/sh
set -eu

export DEBIAN_FRONTEND=noninteractive

ubuntu_snapshot=20260723T000000Z
docker_version=29.1.3-0ubuntu3~24.04.2
containerd_version=2.2.1-0ubuntu1~24.04.3
runc_version=1.3.4-0ubuntu1~24.04.1
apt_sources_name=rv32im-benchmark.sources
apt_source_parts_name=rv32im-benchmark-source-parts
apt_lists_name=rv32im-benchmark-lists
apt_archives_name=rv32im-benchmark-archives
apt_sources=/etc/apt/$apt_sources_name
apt_source_parts=/etc/apt/$apt_source_parts_name
apt_lists=/var/lib/apt/$apt_lists_name
apt_archives=/var/cache/apt/$apt_archives_name
package_record=/var/tmp/rv32im-benchmark-host-packages.txt

. /etc/os-release
[ "${VERSION_CODENAME:-}" = noble ] || {
    echo "benchmark host must be Ubuntu noble" >&2
    exit 1
}
[ "$(dpkg --print-architecture)" = amd64 ] || {
    echo "benchmark host must use the amd64 package architecture" >&2
    exit 1
}

# Do not let background upgrades race or mutate the pinned host package set.
systemctl mask --now \
    apt-daily.service \
    apt-daily.timer \
    apt-daily-upgrade.service \
    apt-daily-upgrade.timer

cat >"$apt_sources" <<EOF
Types: deb
URIs: https://archive.ubuntu.com/ubuntu
Suites: noble noble-updates
Components: main universe
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
Snapshot: $ubuntu_snapshot

Types: deb
URIs: https://security.ubuntu.com/ubuntu
Suites: noble-security
Components: main universe
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
Snapshot: $ubuntu_snapshot
EOF

mkdir -p \
    "$apt_source_parts" \
    "$apt_lists/partial" \
    "$apt_archives/partial"

benchmark_apt_get() {
    apt-get \
        -o "Dir::Etc::sourcelist=$apt_sources_name" \
        -o "Dir::Etc::sourceparts=$apt_source_parts_name" \
        -o "Dir::State::lists=$apt_lists_name" \
        -o "Dir::Cache::archives=$apt_archives_name" \
        "$@"
}

benchmark_apt_get -o APT::Update::Error-Mode=any update
benchmark_apt_get install -y --no-install-recommends --no-remove \
    "docker.io=$docker_version" \
    "containerd=$containerd_version" \
    "runc=$runc_version"

require_package_version() {
    package=$1
    expected=$2
    actual=$(dpkg-query -W -f='${Version}' "$package" 2>/dev/null) || {
        echo "required benchmark host package is missing: $package" >&2
        exit 1
    }
    [ "$actual" = "$expected" ] || {
        echo "benchmark host package $package: expected $expected, got $actual" >&2
        exit 1
    }
}

require_package_version docker.io "$docker_version"
require_package_version containerd "$containerd_version"
require_package_version runc "$runc_version"

{
    printf 'ubuntu_snapshot=%s\n' "$ubuntu_snapshot"
    dpkg-query -W -f='${Package}=${Version}\n' docker.io containerd runc
} >"$package_record"
chmod 0444 "$package_record"

systemctl enable --now docker
systemctl is-active --quiet docker
touch /var/tmp/rv32im-benchmark-ready
