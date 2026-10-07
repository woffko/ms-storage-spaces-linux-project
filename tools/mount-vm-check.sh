#!/bin/bash
# Check how ReFS volumes mount on a Linux test VM through the installed
# package (mount.ReFS, mount -t ReFS, udisks2), one line PASS or FAIL per case.
# Usage: [LINUX_VM_HOST=user@address] tools/mount-vm-check.sh [--deb FILE] [CASE...]
#   --deb FILE  install this package first (dpkg -i, the same version again too)
#   CASE        image loop512 loop4k udisks kill9 wait slow early (default: all)
# The ReFS sample is testdata/refs/r314small (copied to ~/sstest on the VM
# when missing); it is attached through read-only loop devices only.
set -euo pipefail
cd "$(dirname "$0")/.."
deb=
if [[ ${1:-} == --deb ]]; then
  deb=$2
  shift 2
fi
cases=${*:-image loop512 loop4k udisks kill9 wait slow early}
vm() { tools/linux-vm.sh "$@"; }
host=${LINUX_VM_HOST:-192.168.189.142}
[[ $host == *@* ]] || host=codex@$host
ssh_opts=(-o BatchMode=yes -o ConnectTimeout=10 -o IdentitiesOnly=yes -i "$HOME/.ssh/rustadmin_vm_ed25519")
if [[ $host == *@192.168.189.142 ]]; then ssh_opts+=(-o HostKeyAlias=192.168.189.144); fi
if ! vm 'test -f ~/sstest/r314small/disk.img'; then
  vm 'mkdir -p ~/sstest'
  tar -C testdata/refs -cSf - r314small/disk.img | ssh "${ssh_opts[@]}" "$host" 'tar -C ~/sstest -xSf -'
fi
if [[ -n $deb ]]; then
  scp -q "${ssh_opts[@]}" "$deb" "$host:sstest/check.deb"
  vm 'sudo dpkg -i ~/sstest/check.deb >/dev/null && rm ~/sstest/check.deb && dpkg-query -W storage-spaces'
fi
vm "CASES='$cases' bash -s" <<'REMOTE'
set -u
sample=$HOME/sstest/r314small/disk.img
read -r start size < <(sfdisk -d "$sample" | sed -n '/Basic data/s/.*start= *\([0-9]*\), size= *\([0-9]*\).*/\1 \2/p')
off=$((start * 512)) len=$((size * 512))
loops=() pass=0 fail=0
result() {
  if [[ $1 == ok ]]; then echo "PASS $2${3:+ ($3)}"; pass=$((pass + 1)); else echo "FAIL $2: $3"; fail=$((fail + 1)); fi
}
loop() { # loop SECTOR: LOOP = a read-only loop device over the ReFS partition
  LOOP=$(sudo losetup -r -f --show -b "$1" -o "$off" --sizelimit "$len" "$sample")
  loops+=("$LOOP")
}
dir() { sudo mkdir -p "/mnt/mc-$1"; echo "/mnt/mc-$1"; }
fstype() { findmnt -no FSTYPE --mountpoint "$1" 2>/dev/null | tail -1; }
umount_all() {
  for m in /mnt/mc-*; do
    [[ -d $m ]] || continue
    while mountpoint -q "$m"; do sudo umount "$m" 2>/dev/null || sudo umount -l "$m"; done
    sudo rmdir "$m"
  done
}
unit_pid() { # the refs process serving mount point $1 (its mount.ReFS unit or itself)
  local u
  for u in $(systemctl list-units --plain --no-legend 'refs-mount-*' | awk '{print $1}'); do
    if systemctl show -p ExecStart --value "$u" | grep -q " $1 "; then
      systemctl show -p MainPID --value "$u"
      return
    fi
  done
}
cleanup() {
  umount_all
  sudo dmsetup remove mc-slow 2>/dev/null || true
  for l in "${loops[@]}"; do sudo losetup -d "$l" 2>/dev/null || true; done
  sudo rm -f /tmp/mc-fake-refs /tmp/mc-zero.img
}
trap cleanup EXIT

for c in $CASES; do
  case $c in
  image) # mount.ReFS on an image file: refs mounts type fuse.refs itself
    d=$(dir image)
    t0=$SECONDS
    if out=$(sudo /usr/sbin/mount.ReFS "$sample" "$d" 2>&1) && mountpoint -q "$d" && sudo ls "$d" >/dev/null; then
      result ok image "$(fstype "$d"), $((SECONDS - t0)) s"
    else
      result fail image "exit after $((SECONDS - t0)) s, mounted: $(mountpoint -q "$d" && echo yes || echo no); ${out//$'\n'/ | }"
    fi
    umount_all ;;
  loop512 | loop4k) # mount -t ReFS of a block device: fuseblk.refs, root
    sector=512
    [[ $c == loop4k ]] && sector=4096
    loop $sector
    l=$LOOP
    d=$(dir "$c")
    if out=$(sudo mount -t ReFS "$l" "$d" 2>&1) && mountpoint -q "$d" && sudo ls "$d" >/dev/null; then
      opts=$(findmnt -no OPTIONS --mountpoint "$d" | tail -1)
      if [[ $(fstype "$d") != fuseblk.refs ]]; then
        result fail "$c" "type $(fstype "$d")"
      elif [[ $sector == 4096 && $opts != *blksize=4096* ]]; then
        result fail "$c" "no blksize=4096 in $opts"
      else
        result ok "$c" "$(fstype "$d") $opts"
      fi
    else
      result fail "$c" "${out//$'\n'/ | }"
    fi
    umount_all ;;
  udisks) # udisks2 mounts it where it mounts everything (/run/media or /media)
    loop 512
    l=$LOOP
    if out=$(sudo udisksctl mount -b "$l" --no-user-interaction 2>&1); then
      m=$(findmnt -no TARGET --source "$l" | tail -1)
      if [[ -n $m ]] && sudo ls "$m" >/dev/null; then result ok udisks "$m"; else result fail udisks "not mounted: $out"; fi
      sudo udisksctl unmount -b "$l" --no-user-interaction >/dev/null 2>&1 || true
    else
      result fail udisks "${out//$'\n'/ | }"
    fi ;;
  kill9) # killing refs must not leave a dead mount behind
    loop 512
    l=$LOOP
    d=$(dir kill9)
    if sudo mount -t ReFS "$l" "$d" >/dev/null 2>&1 && mountpoint -q "$d"; then
      pid=$(unit_pid "$d")
      if [[ -z $pid || $pid == 0 ]]; then
        result fail kill9 "no refs process found for $d"
      else
        sudo kill -9 "$pid"
        for _ in $(seq 30); do mountpoint -q "$d" || break; sleep 0.1; done
        if mountpoint -q "$d"; then result fail kill9 "still mounted 3 s after kill -9"; else result ok kill9; fi
      fi
    else
      result fail kill9 "could not mount"
    fi
    umount_all ;;
  wait) # a refs that takes 15 s to open the volume: mount.ReFS waits for it
    cat > /tmp/mc-fake-refs <<'FAKE'
#!/bin/bash
# Stands in for "refs mount ... SOURCE DIR": 15 s of opening, then a tmpfs
# mount; it follows --status-file when given.
status= dir=${!#}
while [[ $# -gt 0 ]]; do [[ $1 == --status-file ]] && status=$2; shift; done
for i in $(seq 15); do
  [[ -n $status ]] && printf 'state=opening\nread=%d\n' "$((i * 1000))" > "$status.tmp" && mv "$status.tmp" "$status"
  sleep 1
done
mount -t tmpfs mc-fake "$dir"
[[ -n $status ]] && printf 'state=mounted\n' > "$status.tmp" && mv "$status.tmp" "$status"
exec sleep infinity
FAKE
    sudo chmod 755 /tmp/mc-fake-refs
    d=$(dir wait)
    t0=$SECONDS
    if out=$(sudo REFS=/tmp/mc-fake-refs /usr/sbin/mount.ReFS "$sample" "$d" 2>&1) && mountpoint -q "$d"; then
      result ok wait "mounted after $((SECONDS - t0)) s"
    else
      result fail wait "exit after $((SECONDS - t0)) s, mounted: $(mountpoint -q "$d" && echo yes || echo no); ${out//$'\n'/ | }"
    fi
    pid=$(unit_pid "$d")
    umount_all
    [[ -n $pid && $pid != 0 ]] && sudo kill "$pid" 2>/dev/null
    true ;;
  slow) # a device that answers every read after 1 s: opening the sample
    # (about 30 reads) takes longer than any short timeout would allow
    loop 512
    l=$LOOP
    echo "0 $(sudo blockdev --getsz "$l") delay $l 0 1000" | sudo dmsetup create mc-slow --readonly
    sudo blockdev --flushbufs /dev/mapper/mc-slow
    d=$(dir slow)
    t0=$SECONDS
    if out=$(sudo mount -t ReFS /dev/mapper/mc-slow "$d" 2>&1) && mountpoint -q "$d"; then
      result ok slow "mounted after $((SECONDS - t0)) s"
    else
      result fail slow "exit after $((SECONDS - t0)) s; ${out//$'\n'/ | }"
    fi
    umount_all
    sudo dmsetup remove mc-slow ;;
  early) # no ReFS on the device: a quick, clear failure
    sudo truncate -s 64M /tmp/mc-zero.img
    l=$(sudo losetup -r -f --show /tmp/mc-zero.img)
    loops+=("$l")
    d=$(dir early)
    t0=$SECONDS
    if out=$(sudo mount -t ReFS "$l" "$d" 2>&1); then
      result fail early "mounted a device without ReFS"
    elif [[ $out == *ReFS* && $((SECONDS - t0)) -lt 10 ]]; then
      result ok early "${out//$'\n'/ | }"
    else
      result fail early "after $((SECONDS - t0)) s: ${out//$'\n'/ | }"
    fi
    umount_all ;;
  *) result fail "$c" "unknown case" ;;
  esac
done
echo "$pass passed, $fail failed ($(dpkg-query -W -f='${Package} ${Version}' storage-spaces 2>/dev/null), systemd $(systemctl --version | awk 'NR==1 {print $2}'))"
[[ $fail == 0 ]]
REMOTE
