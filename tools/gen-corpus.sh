#!/bin/bash
# Generate Storage Spaces test pools on the Windows VM.
# Usage: tools/gen-corpus.sh [name...]   (default: every pool in the list)
# Each list line: name followed by New-TestPool.ps1 arguments.
set -uo pipefail
cd "$(dirname "$0")/.."
wanted=" $* "
status=0
while read -r name args; do
  [[ -z $name || $name == \#* ]] && continue
  [[ $# -gt 0 && $wanted != *" $name "* ]] && continue
  echo "=== $name"
  # shellcheck disable=SC2086
  tools/vm.sh -f tools/vm/New-TestPool.ps1 -Name "$name" $args || status=1
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
LIST
exit $status
