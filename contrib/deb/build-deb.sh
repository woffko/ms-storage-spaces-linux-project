#!/bin/bash
# Build a Debian/Ubuntu package from a release build.
# Usage: [BIN_DIR=dir] contrib/deb/build-deb.sh [output dir]
#   (after cargo build --release -p spaces-cli -p refs-cli)
# BIN_DIR is where spaces and refs are (default target/release). A package for
# every distribution wants the static musl build: BIN_DIR=target/x86_64-unknown-linux-musl/release
# (a build on a new distribution links its glibc, which older ones lack).
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
bin=${BIN_DIR:-$root/target/release}
out=${1:-$root/target/deb}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
arch=$(dpkg --print-architecture)
stage=$(mktemp -d)
chmod 755 "$stage"
trap 'rm -rf "$stage"' EXIT

install -D -m 755 "$bin/spaces" "$stage/usr/sbin/spaces"
install -D -m 755 "$bin/refs" "$stage/usr/bin/refs"
install -D -m 755 "$root/contrib/mount.refs" "$stage/usr/sbin/mount.ReFS"
ln -sf mount.ReFS "$stage/usr/sbin/mount.refs"
install -D -m 644 "$root/contrib/udev/69-storage-spaces.rules" "$stage/usr/lib/udev/rules.d/69-storage-spaces.rules"
sed 's#/usr/local/sbin/spaces#/usr/sbin/spaces#' "$root/contrib/systemd/storage-spaces-attach.service" \
  > "$stage/storage-spaces-attach.service"
install -D -m 644 "$stage/storage-spaces-attach.service" "$stage/usr/lib/systemd/system/storage-spaces-attach.service"
rm "$stage/storage-spaces-attach.service"
install -D -m 644 "$root/contrib/man/spaces.8" "$stage/usr/share/man/man8/spaces.8"
gzip -9n "$stage/usr/share/man/man8/spaces.8"
install -D -m 644 "$root/contrib/man/refs.1" "$stage/usr/share/man/man1/refs.1"
gzip -9n "$stage/usr/share/man/man1/refs.1"
install -D -m 644 /dev/stdin "$stage/usr/lib/modules-load.d/storage-spaces.conf" <<<$'ublk_drv\nnbd'
install -D -m 644 "$root/LICENSE" "$stage/usr/share/doc/storage-spaces/copyright"

mkdir -p "$stage/DEBIAN"
cat > "$stage/DEBIAN/control" <<CONTROL
Package: storage-spaces
Version: $version
Architecture: $arch
Maintainer: storage-spaces developers <noreply@example.invalid>
Depends: dmsetup, systemd, udev
Recommends: nbd-client, fuse3
Suggests: ntfs-3g
Section: admin
Homepage: https://github.com/woffko/ms-storage-spaces-linux-project
Priority: optional
Description: read, write and manage Microsoft Storage Spaces pools on Linux
 Assembles Windows 11 Storage Spaces pools from their member disks and
 exposes every virtual disk as a block device under
 /dev/mapper/ss-<pool>-<space>, attached automatically (read-only) when the
 disks appear; spaces can be attached read-write, and pools, spaces and
 disks created and changed the way Windows does. Includes refs, which reads
 ReFS volumes (also inside a space) and mounts them read-only with FUSE.
CONTROL
cat > "$stage/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
  systemctl daemon-reload || true
  systemctl enable storage-spaces-attach.service || true
  udevadm control --reload || true
  modprobe ublk_drv 2>/dev/null || true
fi
POSTINST
cat > "$stage/DEBIAN/prerm" <<'PRERM'
#!/bin/sh
set -e
if [ "$1" = remove ]; then
  spaces detach || true
  systemctl disable storage-spaces-attach.service || true
fi
PRERM
chmod 755 "$stage/DEBIAN/postinst" "$stage/DEBIAN/prerm"
mkdir -p "$out"
dpkg-deb --root-owner-group --build "$stage" "$out/storage-spaces_${version}_${arch}.deb" >/dev/null
echo "$out/storage-spaces_${version}_${arch}.deb"
