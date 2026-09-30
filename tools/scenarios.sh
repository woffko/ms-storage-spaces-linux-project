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
  # A pool field of "-": the steps make the pool themselves (blank, newpool).
  if [[ ${pool// /} != - ]]; then
    # shellcheck disable=SC2086
    run_bg "gen-$name" tools/vm/New-TestPool.ps1 -Name "$name" $pool -Finish Keep || { status=1; continue; }
  fi
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
# Rewriting consistent stripes: does Windows mark them in the journal first?
m5pj3 | -DiskCount 3 -Resiliency Parity -Redundancy 1 -Columns 3 -SizeMB 2048 -NoPattern | snap:s0; write:m5pj3:0:4096:a; snap:s1; write:m5pj3:0:4096:b; snap:s2; dismount
# M7: what a thin space reads where a new slab was not written: a fixed
# space filled with the pattern and deleted, then 4 KiB written into an
# unallocated row of the thin space (whose slab can be one it freed).
m7zero | -DiskCount 2 -Resiliency Simple -Columns 1 -Provisioning Thin -SizeMB 4096 -AllocationUnitMB 256 -NoPattern | newspace:m7fill:Simple:512:Fixed; write:m7fill:0:524288:x; snap:f0:1100; removespace:m7fill; snap:s0:1100; write:m7zero:1048576:4:a; snap:s1:1100; dismount
# M7: the pool database past its 64 formatted slots: 48 writes of 4 KiB,
# 256 MiB apart, all but the first allocating a row of a thin space.
m7grow | -DiskCount 3 -Resiliency Simple -Columns 1 -Provisioning Thin -SizeMB 16384 -AllocationUnitMB 256 -NoPattern | snap:s0:64; writes:m7grow:48:262144:4:g; snap:s1:64; dismount
# M7: TRIM on a thin space: NTFS, a file of 768 MiB (whole slabs),
# deleted, then the free space retrimmed; are slabs given back?
m7trim | -DiskCount 2 -Resiliency Simple -Columns 1 -Provisioning Thin -SizeMB 4096 -AllocationUnitMB 256 -NoPattern | format:m7trim; snap:s0:64; file:m7trim:big:768; snap:s1:64; delfile:m7trim:big; retrim:m7trim; sleep:30; snap:s2:64; dismount
# Stage 3: management operations (docs/plan.md M9-M11).
c9smoke | - | blank:2; snap:b0:64; newpool; snap:p0:64; newspacex:c9x:res=Simple,size=512; snap:p1:64; renamepool:ss-c9smokex; media:0:SSD; usage:1:ManualSelect; snap:p2:64; removespace:c9x; removepool; snap:p3:64; dismount
# Creation: the pool, then one space of each kind (full snapshots).
c9new | - | blank:3; newpool; snap:p0; newspacex:cs:res=Simple,size=1024,prov=Fixed; snap:p1; newspacex:cm:res=Mirror,size=1024,prov=Fixed; snap:p2; newspacex:cp:res=Parity,size=2048,prov=Fixed; snap:p3; newspacex:ct:res=Simple,size=4096,prov=Thin; snap:p4; newspacex:cmt:res=Mirror,size=4096,prov=Thin; snap:p5; newspacex:cw:res=Simple,size=1024,prov=Fixed,wc=64; snap:p6; dismount
# Pools of other sizes and sector sizes.
c9one | - | blank:1; newpool; snap:p0; newspacex:c9ones:res=Simple,size=1024; snap:p1; dismount
c9four | - | blank:4; newpool; snap:p0; dismount
c9eight | - | blank:8; newpool; snap:p0; newspacex:c9eightm:res=Mirror,size=1024; snap:p1; dismount
c9l4k | - | blank:2; newpool:4096; snap:p0; newspacex:c9l4ks:res=Simple,size=1024; snap:p1; dismount
c94kn | - | blank:2:8192:4kn; newpool; snap:p0; newspacex:c94kns:res=Simple,size=1024; snap:p1; dismount
# Pool and disk settings.
c9ops | - | blank:3; newpool; newspacex:c9opsm:res=Mirror,size=1024; snap:o0; renamepool:ss-c9opsx; snap:o1; media:0:SSD; snap:o2; media:1:HDD; usage:2:ManualSelect; snap:o3; usage:2:HotSpare; snap:o4; usage:2:AutoSelect; snap:o5; readonly:true; snap:o6; readonly:false; removespace:c9opsm; snap:o7; removepool; snap:o8; dismount
# Disks added, rebalanced, retired, evacuated and removed, with data.
c9disk | - | blank:3; newpool; newspacex:c9diskm:res=Mirror,size=2048,prov=Fixed; write:c9diskm:0:262144:a; snap:d0; newdisk:3; snap:d1; optimize; snap:d2; retire:0; snap:d3; repair:c9diskm; waitjobs; snap:d4; removedisk:0; waitjobs; snap:d5; dismount
c9drain | - | blank:4; newpool; newspacex:c9drainm:res=Mirror,size=2048,prov=Fixed; write:c9drainm:0:262144:a; snap:r0; removedisk:0; waitjobs; snap:r1; dismount
# Resizing and renaming spaces of each resiliency.
c9resize | - | blank:3; newpool; newspacex:c9rs:res=Simple,size=1024; newspacex:c9rp:res=Parity,size=2048; newspacex:c9rm:res=Mirror,size=1024; snap:z0; resize:c9rp:4096; snap:z1; resize:c9rm:2048; snap:z2; rename:c9rs:c9rs2; snap:z3; dismount
# More creation options, then deleting spaces with hidden parts.
c9opts | - | blank:5; newpool; snap:q0; newspacex:c9m3:res=Mirror,copies=3,size=1024; snap:q1; newspacex:c9p4:res=Parity,cols=4,size=3072,wc=64; snap:q2; newspacex:c9pw0:res=Parity,size=2048,wc=0; snap:q3; dismount
# Mirror and simple spaces take no write-back cache on these pools (c9opts
# stopped there); the remaining options, then deletions and a space after them.
c9opts2 | - | blank:5; newpool; newspacex:c9s1:res=Simple,cols=1,il=64,au=256,size=512; snap:r1; newspacex:c9pt:res=Parity,prov=Thin,size=4096; snap:r2; newspacex:c9p5:res=Parity,size=2048; snap:r3; removespace:c9pt; snap:r4; removespace:c9s1; snap:r5; newspacex:c9after:res=Simple,size=1024; snap:r6; dismount
# What a new space clears of old data on its slabs (c9opts2: its first page).
c9zero | - | blank:2; newpool; newspacex:c9fill:res=Simple,size=2048,prov=Fixed; write:c9fill:0:2097152:x; removespace:c9fill; snap:z0; newspacex:c9z:res=Simple,size=2048,prov=Fixed; snap:z1; dismount
# The pool of tools/health-states.sh as Windows creates it: four disks, a
# one-column and a four-column simple space, a one- and a two-column mirror
# and a parity space (a Linux-created one crashed Windows when it arrived
# without a disk).
c11ctl | - | blank:4; newpool; newspacex:hsimple:res=Simple,cols=1,size=1024; newspacex:hwide:res=Simple,size=1024; newspacex:hmirror:res=Mirror,cols=1,size=1024; newspacex:hmirror2:res=Mirror,size=1024; newspacex:hparity:res=Parity,size=2048; snap:h0; dismount
LIST
exit $status
}
main "$@"
