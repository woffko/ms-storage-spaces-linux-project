#!/bin/bash
# Copy corpus pools to the Linux test VM (/srv/spaces/pools), one pool at a
# time, with resumable rsync. Pools already there are skipped.
# Usage: tools/push-corpus.sh [name...]
set -uo pipefail
tools=$(cd "$(dirname "$0")" && pwd)
cd "$tools/../testdata/pools"
ssh_cmd=(ssh -o BatchMode=yes -o IdentitiesOnly=yes -o ServerAliveInterval=30 -o ServerAliveCountMax=20
  -i "$HOME/.ssh/rustadmin_vm_ed25519" codex@192.168.189.144)
names=("$@")
[[ ${#names[@]} -eq 0 ]] && names=($(ls))
status=0
for name in "${names[@]}"; do
  [[ -f $name/manifest.json ]] || continue
  if "${ssh_cmd[@]}" "test -f /srv/spaces/pools/$name/manifest.json"; then continue; fi
  # rsync resumes interrupted transfers; -z makes the zeros of the sparse
  # images nearly free on the wire.
  ok=0
  for attempt in $(seq 1 15); do
    if rsync -a -z --sparse --partial --append-verify --bwlimit="${BWLIMIT:-8000}" -e "${ssh_cmd[*]:0:${#ssh_cmd[@]}-1}" \
        "$name/" "codex@192.168.189.144:/srv/spaces/pools/.$name.partial/" &&
       "${ssh_cmd[@]}" "mv /srv/spaces/pools/.$name.partial /srv/spaces/pools/$name"; then
      ok=1; echo "pushed $name"; break
    fi
    echo "push of $name interrupted (attempt $attempt), resuming" >&2; sleep 15
  done
  ((ok)) || status=1
done
exit $status
