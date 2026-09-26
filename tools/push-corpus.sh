#!/bin/bash
# Copy corpus pools to the Linux test VM (/srv/spaces/pools), one pool at a
# time, as sparse tar streams. Pools already there are skipped.
# Usage: tools/push-corpus.sh [name...]
set -uo pipefail
cd "$(dirname "$0")/../testdata/pools"
ssh_cmd=(ssh -o BatchMode=yes -o IdentitiesOnly=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=8
  -i "$HOME/.ssh/rustadmin_vm_ed25519" codex@192.168.189.144)
names=("$@")
[[ ${#names[@]} -eq 0 ]] && names=($(ls))
status=0
for name in "${names[@]}"; do
  [[ -f $name/manifest.json ]] || continue
  if "${ssh_cmd[@]}" "test -f /srv/spaces/pools/$name/manifest.json"; then continue; fi
  ok=0
  for attempt in 1 2 3; do
    if tar --sparse -cf - "$name" | "${ssh_cmd[@]}" "set -e; d=/srv/spaces/pools; rm -rf \$d/.$name.partial; mkdir -p \$d/.$name.partial; tar -xf - -C \$d/.$name.partial; mv \$d/.$name.partial/$name \$d/$name; rmdir \$d/.$name.partial"; then
      ok=1; echo "pushed $name"; break
    fi
    echo "push of $name failed (attempt $attempt)" >&2; sleep 20
  done
  ((ok)) || status=1
done
exit $status
