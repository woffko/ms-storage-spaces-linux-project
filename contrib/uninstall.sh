#!/bin/sh
# Remove what contrib/install.sh installed. Attached spaces stay attached
# until `spaces detach` (run it first; it needs the binary this removes).
# Usage: contrib/uninstall.sh [-n]   (run as root; -n only lists what it would do)
set -eu
run() { if [ "${dry:-}" = 1 ]; then echo "would: $*"; else "$@"; fi; }
[ "${1:-}" = -n ] && dry=1
if command -v systemctl >/dev/null 2>&1; then
  run systemctl disable storage-spaces-attach.service 2>/dev/null || true
fi
for f in \
  /usr/local/sbin/spaces \
  /usr/local/bin/refs \
  /sbin/mount.ReFS /sbin/mount.refs \
  /etc/udev/rules.d/69-storage-spaces.rules \
  /etc/systemd/system/storage-spaces-attach.service \
  /etc/modules-load.d/storage-spaces.conf \
  /usr/local/share/man/man8/spaces.8 \
  /usr/local/share/man/man1/refs.1; do
  if [ -e "$f" ] || [ -L "$f" ]; then run rm -f "$f"; fi
done
if command -v systemctl >/dev/null 2>&1; then
  run systemctl daemon-reload
  run udevadm control --reload
fi
echo "removed (the state in /run/storage-spaces is gone at the next boot; the kernel modules stay loaded)"
