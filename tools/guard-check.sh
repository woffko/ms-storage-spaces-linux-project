#!/bin/bash
# Check the health guard of the deb package on a Linux test VM, one line
# PASS, FAIL or SKIP per check:
#   auto     udev attaches a healthy pool and refuses one with a disk away,
#            with its report in /run/storage-spaces/reports, in the journal
#            and in spaces status (also without root)
#   force    a pool whose backup GPT header is gone is refused; --force
#            attaches it read-only, and udev hides its devices and the
#            device it is served through from udisks2; --degraded attaches
#            the pool with a disk away read-only
#   refs     mount -t ReFS and udisksctl refuse a ReFS volume whose log holds
#            changes its checkpoint lacks, saying why and where the report
#            is (udisks2 in its journal); mount -t ReFS -o force mounts it
#            read-only
#   inherit  a ReFS volume in a space attached past its verdict mounts only
#            with -o force
# Usage: [LINUX_VM_HOST=user@address] tools/guard-check.sh DEB [CASE...]
# Pools: testdata/pools/{wc4k,ntfstier} and testdata/refs/{r314integ,
# r314mirror}, copied to the VM when missing (to /srv/spaces where the VM
# has its corpus disk, as compressed tars at 8 MB/s); damaged copies only in
# ~/sstest/scratch there. Where
# an installation by contrib/install.sh exists (its unit and rule in /etc
# shadow the package's), the pools are attached explicitly, and the checks
# of the package's unit and rule are skipped; that installation's spaces
# are left alone.
set -euo pipefail
cd "$(dirname "$0")/.."
deb=${1:?usage: tools/guard-check.sh DEB [CASE...]}
shift
cases=${*:-auto force refs inherit}
host=${LINUX_VM_HOST:-192.168.189.142}
[[ $host == *@* ]] || host=codex@$host
ssh_opts=(-o BatchMode=yes -o ConnectTimeout=10 -o IdentitiesOnly=yes -i "$HOME/.ssh/rustadmin_vm_ed25519")
if [[ $host == *@192.168.189.142 ]]; then ssh_opts+=(-o HostKeyAlias=192.168.189.144); fi
vm() { ssh "${ssh_opts[@]}" "$host" "$@"; }
store=$(vm 'd=/srv/spaces; if [ -d $d ] && [ -w $d ]; then echo $d; else echo sstest; fi')
vm "mkdir -p ~/sstest/pools ~/sstest/refs $store/pools $store/refs"
copy() { # copy NAME KIND: testdata/KIND/NAME to the VM, unless there already
  local name=$1 kind=$2 tarball=target/guard-check/$2-$1.tar.gz
  vm "ls $store/$kind/$name/.complete" >/dev/null 2>&1 || {
    # A compressed tar keeps the holes of the sparse images (tens of GB,
    # little data); rsync resumes it where a dropped connection left it,
    # at 8 MB/s (heavy writes can stall the 22.04 guest's network).
    mkdir -p target/guard-check
    [[ -s $tarball ]] || tar -C "testdata/$kind" -cSzf "$tarball" "$name"
    for attempt in 1 2 3 4 5 6 7 8 9 10; do
      if rsync --partial --bwlimit=8m \
        -e "ssh ${ssh_opts[*]} -o ServerAliveInterval=10 -o ServerAliveCountMax=30" \
        "$tarball" "$host:$store/$kind/.$name.tar.gz"; then
        break
      fi
      echo "copying $name: attempt $attempt failed, again" >&2
      sleep 20
    done
    vm "cd $store/$kind && rm -rf $name && tar -xSzf .$name.tar.gz && rm .$name.tar.gz && touch $name/.complete"
  }
  [[ $store == /* ]] && vm "ln -sfn $store/$kind/$name ~/sstest/$kind/$name"
  return 0
}
copy wc4k pools
copy ntfstier pools
copy r314integ refs
copy r314mirror refs
scp -q "${ssh_opts[@]}" "$deb" "$host:sstest/guard.deb"
vm "CASES='$cases' bash -s" <<'REMOTE'
set -u
pass=0 fail=0
result() {
  if [[ $1 == ok ]]; then echo "PASS $2${3:+ ($3)}"; pass=$((pass + 1));
  elif [[ $1 == skip ]]; then echo "SKIP $2: $3";
  else echo "FAIL $2: $3"; fail=$((fail + 1)); fi
}
one() { printf '%s' "${1//$'\n'/ | }"; }
explicit=0
[[ -e /etc/systemd/system/storage-spaces-attach.service ]] && explicit=1
spaces=/usr/sbin/spaces
pools=~/sstest/pools refsdir=~/sstest/refs scratch=~/sstest/scratch/guard
reports=/run/storage-spaces/reports
loops=()
newloop() { # newloop IMAGE [losetup options]: LOOP = a read-only loop device
  local img=$1
  shift
  LOOP=$(sudo losetup -r -f --show "$@" "$img")
  loops+=("$LOOP")
}
ours() { # dm names of the spaces of the test pools
  sudo sh -c 'cat /run/storage-spaces/*.state' 2>/dev/null |
    sed -n 's/^dm=\(ss-ss_\(wc4k\|ntfstier\|r314mirror\)-[^-]*\)$/\1/p' | sort -u
}
umount_all() {
  for m in /mnt/gc-*; do
    [[ -d $m ]] || continue
    while mountpoint -q "$m"; do sudo umount "$m" 2>/dev/null || sudo umount -l "$m"; done
    sudo rmdir "$m"
  done
}
cleanup() {
  umount_all
  for d in $(ours); do sudo "$spaces" detach "$d" >/dev/null 2>&1 || true; done
  for l in $(losetup -a | grep -E "(sstest|/srv/spaces)/(pools|refs|scratch)/" | cut -d: -f1); do
    sudo losetup -d "$l" 2>/dev/null || true
  done
}
cleanup # what an interrupted run left behind
trap 'cleanup; sudo dpkg -r storage-spaces >/dev/null 2>&1; rm -f ~/sstest/guard.deb' EXIT
sudo dpkg -i ~/sstest/guard.deb >/dev/null
rm -rf "${scratch:?}"
mkdir -p "$scratch/wc4kbad"

# A copy of wc4k whose backup GPT header is overwritten (the last 4 KiB of
# its space of 4 GiB): suspect.
cp --sparse=always $pools/wc4k/disk0.img "$scratch/wc4kbad/disk0.img"
"$spaces" write-pattern "$scratch/wc4kbad/disk0.img" --space wc4k --offset $((4294967296 - 4096)) \
  --length 4096 --tag guard >/dev/null
# ntfstier without one of its disks whose loss leaves the data complete:
# degraded.
away=
for i in 0 1 2 3; do
  rest=$(for j in 0 1 2 3; do [[ $j != "$i" ]] && echo $pools/ntfstier/disk$j.img; done)
  # shellcheck disable=SC2086
  if "$spaces" check --json $rest 2>/dev/null | grep -q '"verdict": "degraded"'; then away=$i; break; fi
done
guid() { # the GUID of space $1 in the pool of the devices that follow
  local name=$1
  shift
  "$spaces" check --json "$@" 2>/dev/null | tr -d '\n ' |
    sed -n "s/.*\"space\":{\"name\":\"$name\",\"guid\":\"\([0-9a-f-]*\)\".*/\1/p"
}
tier_disks=$(for j in 0 1 2 3; do [[ $j != "$away" ]] && echo $pools/ntfstier/disk$j.img; done)
# shellcheck disable=SC2086
tier_guid=$(guid ntfstier $tier_disks)
wc_guid=$(guid wc4k $pools/wc4k/disk0.img)
since=$(date '+%F %T')

for c in $CASES; do
  case $c in
  auto)
    if [[ -z $away ]]; then result fail auto "no disk of ntfstier leaves it degraded"; continue; fi
    if ((explicit)); then
      result skip auto "an install.sh installation's unit shadows the package's here"
      continue
    fi
    # Whole-disk loops with their partitions: udev runs the attach unit.
    newloop $pools/wc4k/disk0.img -P
    for img in $tier_disks; do newloop "$img" -P; done
    for _ in $(seq 60); do
      [[ -n $(ours | grep wc4k) && -e $reports/$tier_guid.txt ]] && break
      sleep 1
    done
    sudo udevadm settle
    if [[ -n $(ours | grep wc4k) ]]; then result ok "auto healthy" "$(ours | grep wc4k | tr '\n' ' ')"; else result fail "auto healthy" "wc4k not attached"; fi
    if [[ -z $(ours | grep ntfstier) ]]; then result ok "auto refused"; else result fail "auto refused" "ntfstier attached"; fi
    r=$(cat "$reports/$tier_guid.txt" 2>/dev/null || true)
    if [[ $r == *DEGRADED* && $r == *pool.members* ]] && [[ $(stat -c %a "$reports/$tier_guid.txt") == 644 ]]; then
      result ok "auto report" "$(head -1 <<<"$r")"
    else
      result fail "auto report" "$(one "$r")"
    fi
    j=$(sudo journalctl -u storage-spaces-attach.service --since "$since" --no-pager -o cat)
    if [[ $j == *"not attached"* && $j == *"DEGRADED pool.members"* ]]; then result ok "auto journal"; else result fail "auto journal" "$(one "$j")"; fi
    s=$("$spaces" status 2>&1)
    if [[ $s == *"not attached: space \"ntfstier\""* && $s == *"verdict healthy"* ]]; then result ok "auto status"; else result fail "auto status" "$(one "$s")"; fi
    cleanup ;;
  force)
    newloop "$scratch/wc4kbad/disk0.img"
    bad=$LOOP
    out=$(sudo "$spaces" attach "$bad" 2>&1)
    if [[ $? != 0 && $out == *"SUSPECT space.partitions"* && $out == *"not attached"* && -z $(ours | grep wc4k) ]]; then
      result ok "force refused" "$(head -1 <<<"$out" | cut -c1-120)"
    else
      result fail "force refused" "$(one "$out")"
    fi
    out=$(sudo "$spaces" attach --force "$bad" 2>&1)
    dm=$(ours | grep wc4k | head -1)
    state=$(sudo cat "/run/storage-spaces/$wc_guid.state" 2>/dev/null || true)
    if [[ -n $dm && $state == *forced=1* && $state == *verdict=suspect* && $out == *read-only* ]]; then
      result ok "force attached" "$dm read-only"
    else
      result fail "force attached" "$(one "$out")"
    fi
    if [[ $(sudo blockdev --getro "/dev/mapper/$dm" 2>/dev/null) == 1 ]]; then result ok "force read-only"; else result fail "force read-only" "/dev/mapper/$dm is writable"; fi
    if ((explicit)); then
      result skip "force udisks" "an install.sh installation's udev rule shadows the package's here"
    else
      sudo udevadm settle
      p=$(udevadm info -q property "/dev/mapper/$dm-p2" 2>&1)
      h=$(udisksctl info -b "/dev/mapper/$dm-p2" 2>&1 | sed -n 's/^ *HintIgnore: *//p')
      if [[ $p == *SS_VERDICT=suspect* && $p == *UDISKS_IGNORE=1* && $h == true ]]; then
        result ok "force udisks" "HintIgnore true"
      else
        result fail "force udisks" "HintIgnore '$h'; $(one "$(grep -E 'SS_|UDISKS' <<<"$p")")"
      fi
      backend=$(sed -n 's/^device=//p' <<<"$state")
      if [[ -n $backend ]]; then
        hidden=yes
        for d in "$backend" "$backend"p*; do
          [[ -b $d ]] || continue
          [[ $(udevadm info -q property "$d") == *UDISKS_IGNORE=1* ]] || hidden="no: $d"
        done
        if [[ $hidden == yes ]]; then result ok "force backend hidden" "$backend and its partitions"; else result fail "force backend hidden" "$hidden"; fi
      fi
    fi
    cleanup
    # A pool with a disk away: read-only with --degraded.
    devs=()
    for img in $tier_disks; do newloop "$img"; devs+=("$LOOP"); done
    out=$(sudo "$spaces" attach --degraded "${devs[@]}" 2>&1)
    state=$(sudo cat "/run/storage-spaces/$tier_guid.state" 2>/dev/null || true)
    if [[ $state == *verdict=degraded* && $state == *forced=1* ]]; then result ok "degraded attached" "read-only"; else result fail "degraded attached" "$(one "$out")"; fi
    cleanup ;;
  refs)
    img=$refsdir/r314integ/disk.img
    read -r start size < <(sfdisk -d "$img" | sed -n '/Basic data/s/.*start= *\([0-9]*\), size= *\([0-9]*\).*/\1 \2/p')
    newloop "$img" -o $((start * 512)) --sizelimit $((size * 512))
    vol=$LOOP
    sudo mkdir -p /mnt/gc-refs
    out=$(sudo mount -t ReFS "$vol" /mnt/gc-refs 2>&1)
    if ! mountpoint -q /mnt/gc-refs && [[ $out == *"SUSPECT: fs.refs.log:"* && $out == *"-o force"* && $out == *"report /run/storage-spaces/reports/refs-"* ]]; then
      result ok "refs refused" "$(grep -m1 SUSPECT <<<"$out" | cut -c1-120)"
    else
      result fail "refs refused" "$(one "$out")"
    fi
    # udisks2 (libblockdev) answers "Unknown error" for whatever a mount
    # helper refuses; the helper's reason goes to udisks2's journal.
    t0=$(date '+%F %T')
    out=$(sudo udisksctl mount -b "$vol" --no-user-interaction 2>&1)
    st=$?
    j=$(sudo journalctl -u udisks2 --since "$t0" --no-pager -o cat)
    if [[ $st != 0 && $j == *"SUSPECT: fs.refs.log:"* && $j == *"report /run/storage-spaces/reports/refs-"* ]]; then
      result ok "refs udisks refused" "udisks2: $(cut -c1-60 <<<"${out##*: }"); its journal: the reason and the report"
    else
      result fail "refs udisks refused" "$(one "$out") / journal: $(one "$j")"
      sudo udisksctl unmount -b "$vol" --no-user-interaction >/dev/null 2>&1 || true
    fi
    out=$(sudo mount -t ReFS -o force "$vol" /mnt/gc-refs 2>&1)
    opts=$(findmnt -no OPTIONS --mountpoint /mnt/gc-refs 2>/dev/null | tail -1)
    if mountpoint -q /mnt/gc-refs && [[ ,$opts, == *,ro,* ]] && sudo ls /mnt/gc-refs >/dev/null; then
      result ok "refs forced" "$opts"
    else
      result fail "refs forced" "$(one "$out") $opts"
    fi
    umount_all
    cleanup ;;
  inherit)
    devs=()
    for img in $refsdir/r314mirror/disk*.img; do newloop "$img"; devs+=("$LOOP"); done
    out=$(sudo "$spaces" attach "${devs[@]}" 2>&1)
    if [[ $out == *"not attached"* && $out == *"fs.refs.log"* ]]; then result ok "inherit refused" "suspect: fs.refs.log"; else result fail "inherit refused" "$(one "$out")"; fi
    sudo "$spaces" attach --force "${devs[@]}" >/dev/null 2>&1
    part=$(ours | grep r314mirror | head -1)-p2
    sudo mkdir -p /mnt/gc-inherit
    out=$(sudo mount -t ReFS "/dev/mapper/$part" /mnt/gc-inherit 2>&1)
    if ! mountpoint -q /mnt/gc-inherit && [[ $out == *"not mounted"* && $out == *"SUSPECT"* ]]; then
      result ok "inherit mount refused"
    else
      result fail "inherit mount refused" "$(one "$out")"
    fi
    r=$(sudo refs check --quick "/dev/mapper/$part" 2>&1)
    if [[ $r == *space.verdict* && $r == *"attached as suspect"* ]]; then result ok "inherit verdict" "space.verdict suspect"; else result fail "inherit verdict" "$(one "$r")"; fi
    out=$(sudo mount -t ReFS -o force "/dev/mapper/$part" /mnt/gc-inherit 2>&1)
    opts=$(findmnt -no OPTIONS --mountpoint /mnt/gc-inherit 2>/dev/null | tail -1)
    if mountpoint -q /mnt/gc-inherit && [[ ,$opts, == *,ro,* ]]; then result ok "inherit forced" "$opts"; else result fail "inherit forced" "$(one "$out")"; fi
    umount_all
    cleanup ;;
  *) result fail "$c" "unknown case" ;;
  esac
done
echo "$pass passed, $fail failed ($(dpkg-query -W -f='${Version}' storage-spaces), systemd $(systemctl --version | awk 'NR==1 {print $2}'))"
[[ $fail == 0 ]]
REMOTE
