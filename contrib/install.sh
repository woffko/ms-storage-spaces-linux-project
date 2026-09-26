#!/bin/sh
# Install the spaces binary, the udev rule and the systemd unit.
# Usage: contrib/install.sh [path/to/spaces]   (run as root)
set -eu
here=$(dirname "$0")
bin=${1:-$here/../target/release/spaces}
install -m 755 "$bin" /usr/local/sbin/spaces
install -m 644 "$here/udev/69-storage-spaces.rules" /etc/udev/rules.d/69-storage-spaces.rules
install -m 644 "$here/systemd/storage-spaces-attach.service" /etc/systemd/system/storage-spaces-attach.service
# Kernel modules the backends use.
printf 'ublk_drv\nnbd\n' > /etc/modules-load.d/storage-spaces.conf
modprobe ublk_drv 2>/dev/null || true
modprobe nbd 2>/dev/null || true
systemctl daemon-reload
systemctl enable storage-spaces-attach.service
udevadm control --reload
echo "installed; pools are attached at boot and when their disks appear"
