#!/bin/bash
# Run commands on the Linux test VM over SSH.
#   tools/linux-vm.sh 'command'   run a command (in ~/Linux_Storage_Spaces if it exists)
#   tools/linux-vm.sh -sync       copy the repository (without target/ and testdata/) to the VM
set -euo pipefail
host=codex@192.168.189.144
ssh_cmd=(ssh -o BatchMode=yes -o ConnectTimeout=5 -o IdentitiesOnly=yes -i "$HOME/.ssh/rustadmin_vm_ed25519")
if [[ ${1:-} == -sync ]]; then
  cd "$(dirname "$0")/.."
  rsync -a --delete --exclude /target/ --exclude /testdata/ --exclude /.git/ \
    -e "${ssh_cmd[*]}" ./ "$host:Linux_Storage_Spaces/"
else
  "${ssh_cmd[@]}" "$host" "cd ~/Linux_Storage_Spaces 2>/dev/null; export PATH=\$HOME/.cargo/bin:\$PATH; $1"
fi
