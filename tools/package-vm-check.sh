#!/bin/bash
# Check the deb package on a Linux test VM, one line PASS or FAIL per case:
#   install  installing it attaches pools whose disks are present, and loads nbd
#   gpt      the attached spaces' partition tables read clean (kernel, sfdisk)
#   user     a user without root sees the attached spaces and why scan sees no disks
#   cycle    its attach unit, enabled, leaves the boot transaction without
#            ordering cycles (systemd-analyze verify, three runs, on a copy of
#            the unit first in the unit path: here as on a VM where another
#            installation's unit shadows it)
#   servers  the servers attach started are not ordered after
#            systemd-tmpfiles-setup.service (at boot they would wait for the
#            attach that waits for them) and have a /tmp of their own
#   busy     spaces detach refuses a space whose volume is mounted, naming the
#            mount, and leaves its server and the volume working
#   upgrade  installing over attached pools (OLD_DEB, else DEB again) attaches
#            the space nothing uses anew with the new program and keeps the
#            one whose volume is mounted on its old server, saying so
#   remove   removing it detaches the spaces its own spaces serves, and only those
#   partitions  a space whose table names 200 partitions (a pool made here,
#            in ~/sstest/scratch/parts): attach makes no devices for them
#            (at most 128), says so and attaches the space, in seconds
#            (1.2.1 made a device for each, two programs and a wait for udev
#            apiece)
# Usage: [LINUX_VM_HOST=user@address] [OLD_DEB=deb] tools/package-vm-check.sh DEB [CASE...]
# Test pools: testdata/pools/{wc4k,ntfstier,sect4k}, copied to ~/sstest/pools
# on the VM when missing, attached through read-only loop devices. Where an
# installation by contrib/install.sh exists (its unit in /etc shadows the
# package's), the pools are attached explicitly with the package's spaces
# (whole-disk loops, which its udev rule leaves alone) and "install" is
# skipped; the spaces that installation serves are left alone throughout.
set -euo pipefail
cd "$(dirname "$0")/.."
deb=${1:?usage: tools/package-vm-check.sh DEB [CASE...]}
shift
cases=${*:-install gpt user cycle servers busy partitions upgrade remove}
host=${LINUX_VM_HOST:-192.168.189.142}
[[ $host == *@* ]] || host=codex@$host
ssh_opts=(-o BatchMode=yes -o ConnectTimeout=10 -o IdentitiesOnly=yes -i "$HOME/.ssh/rustadmin_vm_ed25519")
if [[ $host == *@192.168.189.142 ]]; then ssh_opts+=(-o HostKeyAlias=192.168.189.144); fi
vm() { ssh "${ssh_opts[@]}" "$host" "$@"; }
# The pools go to the VM's corpus disk where it has one (the 22.04 VM's
# system disk is small), linked from ~/sstest/pools; at 15 MB/s, as heavy
# writes can stall the guest's network. sect4k is only the other
# installation's pool, which a VM with an install.sh installation has.
store=$(vm 'd=/srv/spaces/pools; if [ -d $d ] && [ -w $d ]; then echo $d; else echo sstest/pools; fi')
need="wc4k ntfstier"
vm 'test -e /etc/systemd/system/storage-spaces-attach.service' || need+=" sect4k"
vm "mkdir -p ~/sstest/pools"
for pool in $need; do
  # A dropped connection resumes from what arrived (--partial).
  for attempt in 1 2 3 4 5 6 7 8 9 10; do
    vm "ls $store/$pool/.complete" >/dev/null 2>&1 && break
    if rsync -a --sparse --partial --bwlimit=8m \
      -e "ssh ${ssh_opts[*]} -o ServerAliveInterval=10 -o ServerAliveCountMax=30" \
      "testdata/pools/$pool" "$host:$store/"; then
      vm "touch $store/$pool/.complete"
    else
      echo "copying $pool: attempt $attempt failed, again" >&2
      sleep 20
    fi
  done
  [[ $store == /* ]] && vm "ln -sfn $store/$pool ~/sstest/pools/$pool"
done
scp -q "${ssh_opts[@]}" "$deb" "$host:sstest/check.deb"
vm 'rm -f sstest/old.deb'
if [[ -n ${OLD_DEB:-} ]]; then scp -q "${ssh_opts[@]}" "$OLD_DEB" "$host:sstest/old.deb"; fi
vm "CASES='$cases' bash -s" <<'REMOTE'
set -u
pass=0 fail=0
result() {
  if [[ $1 == ok ]]; then echo "PASS $2${3:+ ($3)}"; pass=$((pass + 1));
  elif [[ $1 == skip ]]; then echo "SKIP $2: $3";
  else echo "FAIL $2: $3"; fail=$((fail + 1)); fi
}
explicit=0
[[ -e /etc/systemd/system/storage-spaces-attach.service ]] && explicit=1
other=/opt/sstest/spaces
loops=()
attach_pools() { # attach_pools POOL...: loop devices of the pools' disks (attached by udev or here)
  local p img l new
  for p in "$@"; do
    new=()
    for img in ~/sstest/pools/"$p"/disk*.img; do
      if ((explicit)); then l=$(sudo losetup -r -f --show "$img"); else l=$(sudo losetup -r -f -P --show "$img"); fi
      loops+=("$l") new+=("$l")
    done
    if ((explicit)); then sudo /usr/sbin/spaces attach "${new[@]}" >/dev/null; fi
  done
}
ours() { # dm names of the attached spaces of the test pools
  sudo sh -c 'cat /run/storage-spaces/*.state' 2>/dev/null | sed -n 's/^dm=\(ss-ss_\(wc4k\|ntfstier\|sect4k\|parts\)-[^-]*\)$/\1/p' | sort -u
}
served_by() { # the program the server of attached space $1 (dm name) runs, "" without a server
  local f u
  f=$(sudo sh -c 'grep -lx "dm=$0" /run/storage-spaces/*.state' "$1" 2>/dev/null | head -1)
  u=$(sudo sed -n 's/^unit=//p' "$f" 2>/dev/null)
  [[ -n $u ]] && systemctl show -p ExecStart --value "$u" | sed -n 's/^{ path=\([^ ;]*\).*/\1/p'
}
unit_of() { # the server unit of attached space $1 (dm name)
  local f
  f=$(sudo sh -c 'grep -lx "dm=$0" /run/storage-spaces/*.state' "$1" 2>/dev/null | head -1)
  sudo sed -n 's/^unit=//p' "$f" 2>/dev/null
}
main_pid() { systemctl show -p MainPID --value "$(unit_of "$1")" 2>/dev/null; }
mount_ntfs() { # mount_ntfs DM DIR: the NTFS partition of attached space DM, read-only
  local part
  part=$(lsblk -nro NAME,FSTYPE "/dev/mapper/$1" 2>/dev/null | awk '$2 == "ntfs" {print $1; exit}')
  [[ -n $part ]] || return 1
  sudo mkdir -p "$2"
  sudo mount -o ro "/dev/mapper/$part" "$2" 2>/dev/null || sudo mount -t ntfs3 -o ro "/dev/mapper/$part" "$2" 2>/dev/null ||
    sudo mount -t ntfs-3g -o ro "/dev/mapper/$part" "$2"
}
readable() { # every file of a directory reads
  sudo find "$1" -type f -print0 | sudo xargs -0 -r cat >/dev/null 2>&1 && echo yes || echo no
}
cleanup() { # the test pools' spaces and loop devices, by whichever spaces is there
  local d b l
  for d in /mnt/sstest-busy /mnt/sstest-upgrade; do mountpoint -q "$d" && sudo umount "$d"; done
  for d in $(ours); do
    for b in /usr/sbin/spaces "$other" /usr/local/sbin/spaces ~/sstest/spaces-fixed; do
      [[ -x $b ]] && sudo "$b" detach "$d" >/dev/null 2>&1 && break
    done
  done
  for l in $(losetup -a | grep -E "($HOME/sstest|/srv/spaces)/(pools/(wc4k|ntfstier|sect4k)|scratch/parts)/" | cut -d: -f1); do
    sudo losetup -d "$l" 2>/dev/null || true
  done
  sudo rm -rf /opt/sstest
  rm -rf ~/sstest/scratch/parts
}
cleanup # what an interrupted run left behind
trap 'cleanup; rm -f ~/sstest/check.deb ~/sstest/old.deb' EXIT

since=$(date '+%F %T')
for c in $CASES; do
  case $c in
  install)
    if ((explicit)); then
      sudo dpkg -i ~/sstest/check.deb >/dev/null
      result skip install "an install.sh installation's unit shadows the package's here"
      attach_pools wc4k ntfstier
      continue
    fi
    dpkg -s storage-spaces >/dev/null 2>&1 && sudo dpkg -r storage-spaces >/dev/null
    lsmod | grep -q '^nbd ' && sudo modprobe -r nbd 2>/dev/null
    attach_pools wc4k ntfstier
    sleep 3
    if [[ -n $(ours) ]]; then result fail install "attached before the package was installed"; continue; fi
    sudo dpkg -i ~/sstest/check.deb >/dev/null
    for _ in $(seq 60); do [[ $(ours | wc -l) -ge 2 ]] && break; sleep 1; done
    if [[ $(ours | wc -l) -ge 2 ]]; then result ok install "$(ours | tr '\n' ' ')"; else result fail install "attached after installing: $(ours | tr '\n' ' ')"; fi
    if lsmod | grep -q '^nbd '; then result ok nbd; else result fail nbd "not loaded after installing"; fi ;;
  gpt)
    for d in ss-ss_wc4k-wc4k ss-ss_ntfstier-ntfstier; do
      if [[ ! -b /dev/mapper/$d ]]; then result fail "gpt $d" "not attached"; continue; fi
      out=$(sudo env LC_ALL=C sfdisk --verify "/dev/mapper/$d" 2>&1)
      if [[ $out == *"No errors detected"* && $out != *corrupt* && $out != *invalid* ]]; then result ok "gpt $d" "sfdisk clean"; else result fail "gpt $d" "${out//$'\n'/ | }"; fi
    done
    bad=$(sudo journalctl -k --since "$since" --no-pager -o cat | grep -c 'Alternate GPT is invalid')
    if [[ $bad == 0 ]]; then result ok "gpt kernel"; else result fail "gpt kernel" "$bad times 'Alternate GPT is invalid'"; fi ;;
  user)
    out=$(/usr/sbin/spaces status 2>&1)
    if [[ $out == *ss-ss_wc4k-wc4k* ]]; then result ok "user status"; else result fail "user status" "${out//$'\n'/ | }"; fi
    out=$(/usr/sbin/spaces scan 2>&1)
    if [[ $out == *"permission denied"* ]]; then result ok "user scan" "${out//$'\n'/ | }"; else result fail "user scan" "${out//$'\n'/ | }"; fi ;;
  cycle)
    t=$(mktemp -d)
    mkdir -p "$t/local-fs-pre.target.wants"
    cp /usr/lib/systemd/system/storage-spaces-attach.service "$t/"
    ln -s ../storage-spaces-attach.service "$t/local-fs-pre.target.wants/"
    counts=
    for _ in 1 2 3; do
      counts+="$(SYSTEMD_UNIT_PATH="$t:" systemd-analyze verify default.target 2>&1 | grep -c 'ordering cycle') "
    done
    example=$(SYSTEMD_UNIT_PATH="$t:" systemd-analyze verify default.target 2>&1 | grep -m1 'ordering cycle')
    rm -rf "$t"
    if [[ $counts == "0 0 0 " ]]; then result ok cycle "0 cycles in 3 runs"; else result fail cycle "cycles: $counts: $example"; fi ;;
  servers)
    n=0 bad=
    for d in $(ours); do
      f=$(sudo sh -c 'grep -lx "dm=$0" /run/storage-spaces/*.state' "$d" 2>/dev/null | head -1)
      u=$(sudo sed -n 's/^unit=//p' "$f" 2>/dev/null)
      [[ -n $u ]] || continue
      n=$((n + 1))
      after=$(systemctl show -p After --value "$u")
      tmp=$(systemctl show -p TemporaryFileSystem --value "$u")
      [[ $after == *systemd-tmpfiles-setup* ]] && bad+=" $u: After=$after"
      [[ $tmp == */tmp* ]] || bad+=" $u: TemporaryFileSystem=$tmp"
    done
    if ((n == 0)); then result fail servers "no server of the test pools"
    elif [[ -z $bad ]]; then result ok servers "$n servers"
    else result fail servers "$bad"; fi ;;
  busy)
    d=ss-ss_ntfstier-ntfstier
    if ! mount_ntfs "$d" /mnt/sstest-busy; then result fail busy "cannot mount the NTFS volume of $d"; continue; fi
    unit=$(unit_of "$d")
    out=$(sudo /usr/sbin/spaces detach ntfstier 2>&1)
    rc=$?
    alive=no
    systemctl is-active -q "$unit" && alive=yes
    ok=$(readable /mnt/sstest-busy)
    sudo umount /mnt/sstest-busy
    if [[ $rc != 0 && $out == *"mounted on /mnt/sstest-busy"* && $alive == yes && $ok == yes && -b /dev/mapper/$d ]]; then
      result ok busy "${out//$'\n'/ | }"
    else
      result fail busy "exit $rc, server active: $alive, readable: $ok: ${out//$'\n'/ | }"
    fi ;;
  upgrade)
    if ((explicit)); then result skip upgrade "an install.sh installation's unit shadows the package's here"; continue; fi
    old=~/sstest/check.deb
    [[ -e ~/sstest/old.deb ]] && old=~/sstest/old.deb
    # The test pools attached by the first package's program, a volume of
    # one mounted; then the second package over it.
    for d in $(ours); do sudo /usr/sbin/spaces detach "$d" >/dev/null; done
    sudo dpkg -i "$old" >/dev/null 2>&1
    for _ in $(seq 60); do [[ $(ours | wc -l) -ge 2 ]] && break; sleep 1; done
    w=ss-ss_wc4k-wc4k n=ss-ss_ntfstier-ntfstier
    if ! mount_ntfs "$n" /mnt/sstest-upgrade; then result fail upgrade "cannot mount the NTFS volume of $n"; continue; fi
    pw=$(main_pid "$w") pn=$(main_pid "$n")
    out=$(sudo dpkg -i ~/sstest/check.deb 2>&1)
    for _ in $(seq 60); do p=$(main_pid "$w"); [[ -n $p && $p != 0 && $p != "$pw" ]] && break; sleep 1; done
    ew=$(sudo readlink "/proc/$(main_pid "$w")/exe") en=$(sudo readlink "/proc/$(main_pid "$n")/exe")
    ok=$(readable /mnt/sstest-upgrade)
    sudo umount /mnt/sstest-upgrade
    bad=
    [[ $(main_pid "$w") != "$pw" && $ew == /usr/sbin/spaces ]] || bad+=" $w: server $pw -> $(main_pid "$w") ($ew);"
    [[ $(main_pid "$n") == "$pn" && $en == *"(deleted)" && $ok == yes ]] || bad+=" $n: server $pn -> $(main_pid "$n") ($en), readable: $ok;"
    [[ $out == *"$n is served by the previous version and stays attached"* ]] || bad+=" no notice: ${out//$'\n'/ | }"
    if [[ -z $bad ]]; then result ok upgrade "$(basename "$old") -> check.deb: $w attached anew, $n kept (mounted)"; else result fail upgrade "$bad"; fi ;;
  remove)
    if ((!explicit)); then
      # Another installation's spaces: sect4k, served by a copy elsewhere.
      sudo mkdir -p /opt/sstest && sudo cp /usr/sbin/spaces "$other"
      for img in ~/sstest/pools/sect4k/disk*.img; do loops+=("$(sudo losetup -r -f --show "$img")"); done
      sudo "$other" attach "${loops[@]: -2}" >/dev/null
    fi
    declare -A program=()
    for f in $(sudo ls /run/storage-spaces/ | grep '\.state$'); do
      d=$(sudo sed -n 's/^dm=//p' "/run/storage-spaces/$f" | head -1)
      program[$d]=$(served_by "$d")
    done
    sudo dpkg -r storage-spaces >/dev/null
    bad=
    for d in "${!program[@]}"; do
      attached=no
      [[ -b /dev/mapper/$d ]] && attached=yes
      if [[ ${program[$d]} == /usr/sbin/spaces && $attached == yes ]]; then bad+=" $d (still attached)"; fi
      if [[ ${program[$d]} != /usr/sbin/spaces && -n ${program[$d]} && $attached == no ]]; then bad+=" $d (detached, served by ${program[$d]})"; fi
    done
    if [[ -z $bad ]]; then
      result ok remove "$(for d in "${!program[@]}"; do printf '%s:%s ' "$d" "${program[$d]##*/}"; [[ -b /dev/mapper/$d ]] && printf 'kept ' || printf 'detached '; done)"
    else
      result fail remove "$bad"
    fi ;;
  partitions)
    d=~/sstest/scratch/parts
    rm -rf "${d:?}" && mkdir -p "$d" && truncate -s 3G "$d/disk0.img"
    if ! /usr/sbin/spaces pool create --name ss_parts --yes "$d/disk0.img" >/dev/null 2>&1 ||
      ! /usr/sbin/spaces space create --name parts --resiliency simple --size 1G --yes "$d/disk0.img" >/dev/null 2>&1; then
      result fail partitions "cannot make the pool"
      continue
    fi
    l=$(sudo losetup -f --show "$d/disk0.img")
    # Only its own pool goes afterwards: the later cases need the others.
    drop_parts() {
      for x in $(ours | grep parts); do sudo /usr/sbin/spaces detach "$x" >/dev/null 2>&1; done
      sudo losetup -d "$l" 2>/dev/null
      rm -rf "${d:?}"
    }
    sudo /usr/sbin/spaces attach --rw --space parts "$l" >/dev/null 2>&1
    dm=$(ours | grep parts | head -1)
    if [[ -z $dm ]]; then result fail partitions "cannot attach it for writing"; drop_parts; continue; fi
    { echo "label: gpt"; echo "table-length: 256"
      for _ in $(seq 200); do echo "size=1MiB, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"; done
    } | sudo sfdisk -q "/dev/mapper/$dm" >/dev/null 2>&1
    sudo /usr/sbin/spaces detach "$dm" >/dev/null 2>&1
    t0=$SECONDS
    out=$(sudo /usr/sbin/spaces attach --space parts "$l" 2>&1)
    took=$((SECONDS - t0))
    devices=$(find /dev/mapper -name "$dm-p*" | wc -l)
    if [[ $out == *"no devices for its partitions: 200 partitions"* && -b /dev/mapper/$dm && $devices == 0 && $took -lt 60 ]]; then
      result ok partitions "200 partitions, no devices for them, the space attached in ${took} s"
    else
      result fail partitions "$(echo "$out" | tr '\n' ' ') ($devices partition devices, ${took} s)"
    fi
    drop_parts ;;
  *) result fail "$c" "unknown case" ;;
  esac
done
echo "$pass passed, $fail failed (systemd $(systemctl --version | awk 'NR==1 {print $2}'))"
[[ $fail == 0 ]]
REMOTE
