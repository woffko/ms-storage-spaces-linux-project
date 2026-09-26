#!/bin/bash
# Run PowerShell on the Windows test VM over SSH.
#   tools/vm.sh 'Get-StoragePool'             run an inline script
#   tools/vm.sh -f script.ps1 [args...]       upload script to C:\sstest and run it
#   tools/vm.sh -get REMOTE_PATH LOCAL_PATH   copy a file from the VM
set -euo pipefail
host=root@192.168.189.129
ssh_opts=(-F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes
  -o StrictHostKeyChecking=yes -o HostKeyAlias=192.168.189.138
  -o ConnectTimeout=5 -i "$HOME/.ssh/rustadmin_vm_ed25519")

run_ps() {
  # Errors are rendered as text and turn into a non-zero exit code.
  local wrapped="\$ProgressPreference='SilentlyContinue'; try { & { $1 } 2>&1 | Out-String -Stream -Width 250 } catch { Write-Output (\"ERROR: \" + \$_); exit 1 }"
  local enc
  enc=$(printf '%s' "$wrapped" | iconv -t UTF-16LE | base64 -w0)
  ssh -n "${ssh_opts[@]}" "$host" "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand $enc"
}

case "${1:-}" in
  -f)
    script=$2; shift 2
    name=$(basename "$script")
    scp -q "${ssh_opts[@]}" "$script" "$host:C:/sstest/$name"
    args=""
    for a in "$@"; do
      if [[ $a =~ ^-[A-Za-z]+$ ]]; then args+=" $a"; else args+=" '${a//\'/\'\'}'"; fi
    done
    run_ps "\$ErrorActionPreference='Stop'; & 'C:\\sstest\\$name'$args"
    ;;
  -get)
    scp -q "${ssh_opts[@]}" "$host:$2" "$3"
    ;;
  *)
    run_ps "$1"
    ;;
esac
