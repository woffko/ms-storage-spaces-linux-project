#!/bin/bash
# Scenarios for the experiments of M5 (docs/plan.md): a pool made by
# tools/vm/New-TestPool.ps1 -Finish Keep, the steps tools/vm/Invoke-Scenario.ps1
# runs on it, and the snapshots it takes, fetched into testdata/snapshots/.
# Usage: tools/scenarios.sh NAME...
set -uo pipefail
cd "$(dirname "$0")/.."

# Starts a script detached on the VM and waits for its "EXIT" line.
run_bg() {
  local log=$1; shift
  local started=0
  for attempt in 1 2 3; do
    if tools/vm.sh -bg "$log" "$@" </dev/null; then started=1; break; fi
    sleep 30
  done
  ((started)) || return 1
  while :; do
    sleep 15
    out=$(timeout 60 tools/vm.sh "Get-Content C:\\sstest\\logs\\$log.log -Tail 40" </dev/null 2>/dev/null) || continue
    if grep -q '^EXIT ' <<<"$out"; then
      grep -v '^\s*$' <<<"$out" | tail -25
      grep -q '^EXIT 0' <<<"$out"
      return
    fi
  done
}

# The whole run is one function, so bash has read it before it starts and
# editing the list while scenarios run is safe.
main() {
status=0
wanted=" $* "
while IFS='|' read -r name pool steps; do
  name=${name// /}
  [[ -z $name || $name == \#* ]] && continue
  [[ $wanted != *" $name "* ]] && continue
  echo "=== $name"
  # shellcheck disable=SC2086
  run_bg "gen-$name" tools/vm/New-TestPool.ps1 -Name "$name" $pool -Finish Keep || { status=1; continue; }
  run_bg "scn-$name" tools/vm/Invoke-Scenario.ps1 -Name "$name" -Steps "${steps// /}" || { status=1; continue; }
  labels=$(grep -o 'snap:[A-Za-z0-9_-]*' <<<"$steps" | cut -d: -f2)
  # shellcheck disable=SC2086
  tools/fetch-snapshot.sh "$name" $labels || status=1
done <<'LIST'
# The mirror dirty region log through writes into four extent runs of
# 256 MiB, a disconnect and a reconnect.
m5drt | -DiskCount 2 -Resiliency Mirror -DataCopies 2 -Columns 1 -SizeMB 1024 -AllocationUnitMB 256 -NoPattern | snap:s0; write:m5drt:0:4:a; snap:s1; write:m5drt:524288:4:b; snap:s2; write:m5drt:4096:4:c; snap:s3; write:m5drt:262144:4:d; write:m5drt:786432:4:e; snap:s4; disconnect:m5drt; snap:s5; connect:m5drt; snap:s6; write:m5drt:262144:4:f; snap:s7; dismount
# How long a run stays listed: a write into the next run after 5 to 180 s.
m5drt2 | -DiskCount 2 -Resiliency Mirror -DataCopies 2 -Columns 1 -SizeMB 4096 -AllocationUnitMB 256 -NoPattern | write:m5drt2:0:4:r0; snap:a0:1100; sleep:5; write:m5drt2:262144:4:r1; snap:a1:1100; sleep:30; write:m5drt2:524288:4:r2; snap:a2:1100; sleep:60; write:m5drt2:786432:4:r3; snap:a3:1100; sleep:90; write:m5drt2:1048576:4:r4; snap:a4:1100; sleep:120; write:m5drt2:1310720:4:r5; snap:a5:1100; sleep:180; write:m5drt2:1572864:4:r6; snap:a6:1100; dismount
# Pool database updates: rename, a new space, extension, deletion, a
# read-only pool.
m5db | -DiskCount 2 -Resiliency Simple -Columns 2 -SizeMB 1024 -AllocationUnitMB 256 -NoPattern | snap:s0:1100; rename:m5db:m5dbx; snap:s1:1100; newspace:m5dbn:Simple:512; snap:s2:1100; resize:m5dbx:1536; snap:s3:1100; removespace:m5dbn; snap:s4:1100; readonly:true; snap:s5:1100; readonly:false; snap:s6:1100; dismount
# Slab allocation of thin spaces: writes in an order unlike the offsets.
m5thin | -DiskCount 3 -Resiliency Simple -Columns 1 -Provisioning Thin -SizeMB 4096 -AllocationUnitMB 256 -NoPattern | snap:s0:1100; write:m5thin:2097152:4:a; snap:s1:1100; write:m5thin:0:4:b; snap:s2:1100; write:m5thin:3145728:4:c; snap:s3:1100; write:m5thin:262144:4:d; snap:s4:1100; write:m5thin:1048576:1048576:e; snap:s5:1100; dismount
m5thinm | -DiskCount 3 -Resiliency Mirror -DataCopies 2 -Columns 1 -Provisioning Thin -SizeMB 4096 -AllocationUnitMB 256 -NoPattern | snap:s0:1100; write:m5thinm:2097152:4:a; snap:s1:1100; write:m5thinm:0:4:b; snap:s2:1100; write:m5thinm:3145728:4:c; snap:s3:1100; write:m5thinm:1048576:1048576:e; snap:s4:1100; dismount
# A write that misses a disk, the disk returning, and repair.
m5stale | -DiskCount 2 -Resiliency Mirror -DataCopies 2 -Columns 1 -SizeMB 1024 -AllocationUnitMB 256 -NoPattern | snap:s0:1100; write:m5stale:0:4:a; snap:s1:1100; detachdisk:1; write:m5stale:4096:4:b; snap:s2:1100; write:m5stale:524288:4:c; snap:s3:1100; attachdisk:1; sleep:10; snap:s4:1100; repair:m5stale; snap:s5:1100; dismount
# The write path of the write-back cache and of the parity journal.
m5wbc | -DiskCount 3 -Resiliency Parity -Redundancy 1 -Columns 3 -SizeMB 2048 -NoPattern | snap:s0; write:m5wbc:0:4:a; snap:s1; write:m5wbc:1024:64:b; snap:s2; sleep:30; snap:s3; write:m5wbc:0:4:c; snap:s4; sleep:120; snap:s5; dismount
m5pj | -DiskCount 3 -Resiliency Parity -Redundancy 1 -Columns 3 -SizeMB 2048 -WriteCacheMB 0 -NoPattern | snap:s0; write:m5pj:0:4:a; snap:s1; write:m5pj:1024:64:b; snap:s2; sleep:30; snap:s3; write:m5pj:0:512:c; snap:s4; dismount
# The parity journal: writes of whole stripes and more, which bypass the cache.
m5pj2 | -DiskCount 3 -Resiliency Parity -Redundancy 1 -Columns 3 -SizeMB 2048 -NoPattern | snap:s0; write:m5pj2:0:4096:a; snap:s1; write:m5pj2:8192:512:b; snap:s2; write:m5pj2:16384:4096:c; snap:s3; sleep:60; snap:s4; write:m5pj2:0:4:d; snap:s5; dismount
# m5thin once more: whether Windows picks the same disks.
m5thin2 | -DiskCount 3 -Resiliency Simple -Columns 1 -Provisioning Thin -SizeMB 4096 -AllocationUnitMB 256 -NoPattern | snap:s0:1100; write:m5thin2:2097152:4:a; snap:s1:1100; write:m5thin2:0:4:b; snap:s2:1100; write:m5thin2:3145728:4:c; snap:s3:1100; write:m5thin2:262144:4:d; snap:s4:1100; write:m5thin2:1048576:1048576:e; snap:s5:1100; dismount
# Object ids of new spaces: three created, one deleted in between.
m5ids | -DiskCount 2 -Resiliency Simple -Columns 2 -SizeMB 1024 -AllocationUnitMB 256 -NoPattern | snap:s0:1100; newspace:m5idsa:Simple:512; snap:s1:1100; newspace:m5idsb:Mirror:512; snap:s2:1100; removespace:m5idsa; newspace:m5idsc:Simple:512; snap:s3:1100; dismount
# Destaging of the write-back cache: a disconnect, then 1500 small writes
# into distinct chunks (73 % of the cache), then five minutes idle.
m5wbc2 | -DiskCount 3 -Resiliency Parity -Redundancy 1 -Columns 3 -SizeMB 2048 -NoPattern | snap:s0; write:m5wbc2:0:4:a; snap:s1; disconnect:m5wbc2; snap:s2; connect:m5wbc2; snap:s3; writes:m5wbc2:1500:512:4:b; snap:s4; sleep:300; snap:s5; dismount
# Thin parity space with the cache: a write into an unallocated row, then
# enough small writes to make Windows destage.
m5thinwbc | -DiskCount 3 -Resiliency Parity -Redundancy 1 -Columns 3 -Provisioning Thin -SizeMB 4096 -NoPattern | snap:s0; write:m5thinwbc:2097152:4:a; snap:s1; writes:m5thinwbc:1500:512:4:b; snap:s2; dismount
LIST
exit $status
}
main "$@"
