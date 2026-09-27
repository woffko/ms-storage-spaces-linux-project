#!/bin/bash
# Generate Storage Spaces test pools on the Windows VM.
# Usage: tools/gen-corpus.sh [name...]   (default: every pool in the list)
# Each list line: name followed by New-TestPool.ps1 arguments, or by
# @Script.ps1 and that script's arguments. SUFFIX=_26100 appends a suffix to
# the pool names (the same configurations created by another Windows build).
set -uo pipefail
cd "$(dirname "$0")/.."
wanted=" $* "
status=0
while read -r name args; do
  [[ -z $name || $name == \#* ]] && continue
  [[ $# -gt 0 && $wanted != *" $name "* ]] && continue
  script=tools/vm/New-TestPool.ps1
  if [[ $args == @* ]]; then
    script=tools/vm/${args%% *}; script=${script/@/}
    [[ $args == *" "* ]] && args=${args#* } || args=
  fi
  name+=${SUFFIX:-}
  echo "=== $name"
  # The generator runs detached on the VM: heavy I/O there can stall the
  # guest network for a minute, which would kill an attached SSH session.
  # Starting twice is harmless (the generator refuses an existing pool).
  started=0
  for attempt in 1 2 3; do
    # shellcheck disable=SC2086
    if tools/vm.sh -bg "gen-$name" "$script" -Name "$name" $args </dev/null; then started=1; break; fi
    sleep 30
  done
  if ((!started)); then status=1; continue; fi
  while :; do
    sleep 20
    out=$(timeout 60 tools/vm.sh "Get-Content C:\\sstest\\logs\\gen-$name.log -Tail 20" </dev/null 2>/dev/null) || continue
    if grep -q '^EXIT ' <<<"$out"; then
      grep -v '^\s*$' <<<"$out" | tail -4
      grep -q '^EXIT 0' <<<"$out" || status=1
      break
    fi
  done
done <<'LIST'
# Batch 1: basic layouts (2026-09-26)
simple1c   -DiskCount 1 -Resiliency Simple -Columns 1 -SizeMB 1024
simple2c   -DiskCount 2 -Resiliency Simple -Columns 2 -InterleaveKB 64 -SizeMB 1024
simple3c   -DiskCount 3 -Resiliency Simple -Columns 3 -InterleaveKB 256 -SizeMB 1536
mirror2    -DiskCount 2 -Resiliency Mirror -DataCopies 2 -Columns 1 -SizeMB 1024
mirror2x2c -DiskCount 4 -Resiliency Mirror -DataCopies 2 -Columns 2 -InterleaveKB 64 -SizeMB 1024
mirror3    -DiskCount 5 -Resiliency Mirror -DataCopies 3 -Columns 1 -SizeMB 1024
parity3    -DiskCount 3 -Resiliency Parity -Redundancy 1 -Columns 3 -InterleaveKB 64 -SizeMB 1024
thin2c     -DiskCount 2 -Resiliency Simple -Columns 2 -InterleaveKB 256 -Provisioning Thin -SizeMB 16384 -PatternMB 768
# Batch 2: sector size, allocation unit, cache size, wider parity, thin mirror/parity
sect4k     -DiskCount 2 -Resiliency Simple -Columns 2 -InterleaveKB 64 -SizeMB 1024 -LogicalSectorSize 4096
sect512    -DiskCount 2 -Resiliency Simple -Columns 2 -InterleaveKB 64 -SizeMB 1024 -LogicalSectorSize 512
au1g       -DiskCount 1 -Resiliency Simple -Columns 1 -Provisioning Thin -SizeMB 8192 -AllocationUnitMB 1024 -PatternMB 1536
nocache    -DiskCount 2 -Resiliency Simple -Columns 2 -Provisioning Thin -SizeMB 8192 -WriteCacheMB 0 -PatternMB 768
wc64       -DiskCount 2 -Resiliency Simple -Columns 2 -Provisioning Thin -SizeMB 8192 -WriteCacheMB 64 -PatternMB 768
parity4    -DiskCount 4 -Resiliency Parity -Redundancy 1 -Columns 4 -InterleaveKB 256 -SizeMB 1536
parity5    -DiskCount 5 -Resiliency Parity -Redundancy 1 -Columns 5 -InterleaveKB 64 -SizeMB 2048
dual7      -DiskCount 7 -Resiliency Parity -Redundancy 2 -Columns 7 -InterleaveKB 64 -SizeMB 2560
mirrorthin -DiskCount 2 -Resiliency Mirror -DataCopies 2 -Columns 1 -Provisioning Thin -SizeMB 8192 -PatternMB 768
paritythin -DiskCount 3 -Resiliency Parity -Redundancy 1 -Columns 3 -Provisioning Thin -SizeMB 8192 -PatternMB 768
# Batch 3: storage tiers
tiered     -DiskCount 4 -SsdDisks 2 -Tiers SSD,Mirror,1024;HDD,Simple,1024,2 -Provisioning Fixed
mapar      -DiskCount 5 -SsdDisks 2 -Tiers SSD,Mirror,1024;HDD,Parity,2048,3 -Provisioning Fixed
# Batch 4: dual parity codes; impulse pools go to testdata/impulse
# (fetch-corpus.sh --impulse) and become fixtures under tests/data
imp7b      @New-ImpulsePool.ps1 -Columns 7
imp8       @New-ImpulsePool.ps1 -Columns 8 -Simple
imp9       @New-ImpulsePool.ps1 -Columns 9 -Simple
imp10      @New-ImpulsePool.ps1 -Columns 10 -Simple
lrc11      -DiskCount 11 -Resiliency Parity -Redundancy 2 -Columns 11 -InterleaveKB 64 -SizeMB 2048 -WriteCacheMB 0 -PatternMB 1024
lrc12      -DiskCount 12 -Resiliency Parity -Redundancy 2 -Columns 12 -InterleaveKB 64 -SizeMB 2304 -WriteCacheMB 0
lrc12i     @New-ImpulsePool.ps1 -Columns 12 -DataColumns 9 -Simple -FlushMB 2048
lrc17i     @New-ImpulsePool.ps1 -Columns 17 -DataColumns 13 -Simple -FlushMB 2048
LIST
exit $status
