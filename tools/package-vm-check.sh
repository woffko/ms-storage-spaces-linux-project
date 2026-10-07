#!/bin/bash
# Check the deb package on a Linux test VM, one line PASS or FAIL per case:
#   install  installing it attaches pools whose disks are present, and loads nbd
#   gpt      the attached spaces' partition tables read clean (kernel, sfdisk)
#   user     a user without root sees the attached spaces and why scan sees no disks
#   remove   removing it detaches the spaces its own spaces serves, and only those
# Usage: [LINUX_VM_HOST=user@address] tools/package-vm-check.sh DEB [CASE...]
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
cases=${*:-install gpt user remove}
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
  sudo sh -c 'cat /run/storage-spaces/*.state' 2>/dev/null | sed -n 's/^dm=\(ss-ss_\(wc4k\|ntfstier\|sect4k\)-[^-]*\)$/\1/p' | sort -u
}
served_by() { # the program the server of attached space $1 (dm name) runs, "" without a server
  local f u
  f=$(sudo sh -c 'grep -lx "dm=$0" /run/storage-spaces/*.state' "$1" 2>/dev/null | head -1)
  u=$(sudo sed -n 's/^unit=//p' "$f" 2>/dev/null)
  [[ -n $u ]] && systemctl show -p ExecStart --value "$u" | sed -n 's/^{ path=\([^ ;]*\).*/\1/p'
}
cleanup() { # the test pools' spaces and loop devices, by whichever spaces is there
  local d b l
  for d in $(ours); do
    for b in /usr/sbin/spaces "$other" /usr/local/sbin/spaces ~/sstest/spaces-fixed; do
      [[ -x $b ]] && sudo "$b" detach "$d" >/dev/null 2>&1 && break
    done
  done
  for l in $(losetup -a | grep -E "($HOME/sstest|/srv/spaces)/pools/(wc4k|ntfstier|sect4k)/" | cut -d: -f1); do
    sudo losetup -d "$l" 2>/dev/null || true
  done
  sudo rm -rf /opt/sstest
}
cleanup # what an interrupted run left behind
trap 'cleanup; rm -f ~/sstest/check.deb' EXIT

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
  *) result fail "$c" "unknown case" ;;
  esac
done
echo "$pass passed, $fail failed (systemd $(systemctl --version | awk 'NR==1 {print $2}'))"
[[ $fail == 0 ]]
REMOTE
