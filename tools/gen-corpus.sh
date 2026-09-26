#!/bin/bash
# Generate the baseline Storage Spaces test corpus on the Windows VM.
# Each line: name followed by New-TestPool.ps1 arguments.
set -uo pipefail
cd "$(dirname "$0")/.."
while read -r name args; do
  [[ -z $name || $name == \#* ]] && continue
  echo "=== $name"
  # shellcheck disable=SC2086
  tools/vm.sh -f tools/vm/New-TestPool.ps1 -Name "$name" $args
done <<'LIST'
simple1c   -DiskCount 1 -Resiliency Simple -Columns 1 -SizeMB 1024
simple2c   -DiskCount 2 -Resiliency Simple -Columns 2 -InterleaveKB 64 -SizeMB 1024
simple3c   -DiskCount 3 -Resiliency Simple -Columns 3 -InterleaveKB 256 -SizeMB 1536
mirror2    -DiskCount 2 -Resiliency Mirror -DataCopies 2 -Columns 1 -SizeMB 1024
mirror2x2c -DiskCount 4 -Resiliency Mirror -DataCopies 2 -Columns 2 -InterleaveKB 64 -SizeMB 1024
mirror3    -DiskCount 5 -Resiliency Mirror -DataCopies 3 -Columns 1 -SizeMB 1024
parity3    -DiskCount 3 -Resiliency Parity -Redundancy 1 -Columns 3 -InterleaveKB 64 -SizeMB 1024
thin2c     -DiskCount 2 -Resiliency Simple -Columns 2 -InterleaveKB 256 -Provisioning Thin -SizeMB 16384 -PatternMB 768
LIST
