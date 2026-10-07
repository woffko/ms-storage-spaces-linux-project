#!/bin/sh
# Install the spaces binary, the udev rule and the systemd unit (and refs
# when it was built next to spaces).
# Usage: contrib/install.sh [path/to/spaces]   (run as root)
set -eu
here=$(dirname "$0")
bin=${1:-$here/../target/release/spaces}
install -m 755 "$bin" /usr/local/sbin/spaces
install -m 644 "$here/udev/69-storage-spaces.rules" /etc/udev/rules.d/69-storage-spaces.rules
install -m 644 "$here/systemd/storage-spaces-attach.service" /etc/systemd/system/storage-spaces-attach.service
install -D -m 644 "$here/man/spaces.8" /usr/local/share/man/man8/spaces.8
if [ -x "$(dirname "$bin")/refs" ]; then
  install -m 755 "$(dirname "$bin")/refs" /usr/local/bin/refs
  install -D -m 644 "$here/man/refs.1" /usr/local/share/man/man1/refs.1
  # mount -t ReFS (the type blkid reports), fstab and udisks2 mount ReFS
  # through it.
  install -m 755 "$here/mount.refs" /sbin/mount.ReFS
  ln -sf mount.ReFS /sbin/mount.refs
fi
# Kernel modules the backends use.
printf 'ublk_drv\nnbd\n' > /etc/modules-load.d/storage-spaces.conf
modprobe ublk_drv 2>/dev/null || true
modprobe nbd 2>/dev/null || true
systemctl daemon-reload
systemctl enable storage-spaces-attach.service
udevadm control --reload
# The udev rule acts on disks that appear later: attach the pools whose
# disks are here now.
systemctl start --no-block storage-spaces-attach.service
echo "installed; pools whose disks are here are being attached (journalctl -u storage-spaces-attach),"
echo "the others at boot and when their disks appear"
