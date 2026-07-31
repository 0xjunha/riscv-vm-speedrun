#!/bin/sh
set -eu

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends docker.io
systemctl enable --now docker
touch /var/tmp/rv32im-benchmark-ready
