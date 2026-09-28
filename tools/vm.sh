#!/bin/bash
# Run PowerShell on the Windows test VM over SSH.
#   tools/vm.sh 'Get-StoragePool'             run an inline script
#   tools/vm.sh -f script.ps1 [args...]       upload script to C:\sstest and run it
#   tools/vm.sh -bg LOG script.ps1 [args...]  upload and start the script detached from
#                                             the SSH session (survives network drops);
#                                             output goes to C:\sstest\logs\LOG.log, and a
#                                             final "EXIT <code>" line marks completion
#   tools/vm.sh -get REMOTE_PATH LOCAL_PATH   copy a file from the VM
#   tools/vm.sh -put LOCAL_PATH REMOTE_PATH   copy a file to the VM
#
# The VM is DESKTOP-BQ2J4NS (Windows 11 24H2, sshuser@10.0.77.97, key login
# as a member of Administrators). The previous VM DESKTOP-ELS4LDK (Insider
# build 26340, which created the first corpus) is reached with
# WIN_VM_HOST=root@192.168.189.129 WIN_VM_HOSTKEY_ALIAS=192.168.189.138.
set -euo pipefail
host=${WIN_VM_HOST:-sshuser@10.0.77.97}
ssh_opts=(-F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes
  -o StrictHostKeyChecking=yes -o HostKeyAlias="${WIN_VM_HOSTKEY_ALIAS:-${host#*@}}"
  -o ConnectTimeout=30 -i "$HOME/.ssh/rustadmin_vm_ed25519")

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
      if [[ $a =~ ^-[A-Za-z][A-Za-z0-9]*$ ]]; then args+=" $a"; else args+=" '${a//\'/\'\'}'"; fi
    done
    run_ps "\$ErrorActionPreference='Stop'; & 'C:\\sstest\\$name'$args"
    ;;
  -bg)
    log=$2; script=$3; shift 3
    [[ $log =~ ^[A-Za-z0-9_-]+$ ]] || { echo "bad log name" >&2; exit 1; }
    name=$(basename "$script")
    scp -q "${ssh_opts[@]}" "$script" "$host:C:/sstest/$name"
    args=""
    for a in "$@"; do
      if [[ $a =~ ^-[A-Za-z][A-Za-z0-9]*$ ]]; then args+=" $a"; else args+=" '${a//\'/\'\'}'"; fi
    done
    # The inner script runs in a process created by WMI, outside the job
    # object of the SSH session, so it survives disconnects.
    inner="\$ProgressPreference='SilentlyContinue'; \$ErrorActionPreference='Stop'; try { & 'C:\\sstest\\$name'$args *>&1 | Out-String -Stream -Width 250; \$c=0 } catch { 'ERROR: ' + \$_; \$c=1 }; \"EXIT \$c\""
    enc=$(printf '%s' "$inner" | iconv -t UTF-16LE | base64 -w0)
    run_ps "New-Item -ItemType Directory -Force C:\\sstest\\logs | Out-Null; if (Test-Path C:\\sstest\\logs\\$log.log) { 'already started'; return }; \$cmd = 'cmd.exe /c powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand $enc > C:\\sstest\\logs\\$log.log 2>&1'; \$r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = \$cmd }; if (\$r.ReturnValue -ne 0) { throw \"start failed: \$(\$r.ReturnValue)\" }; \"started pid \$(\$r.ProcessId)\""
    ;;
  -get)
    # The VM drops connections under load; retry a few times.
    for attempt in 1 2 3 4 5 6; do
      if scp -q "${ssh_opts[@]}" -o ServerAliveInterval=15 "$host:$2" "$3"; then exit 0; fi
      echo "scp attempt $attempt failed, retrying" >&2
      sleep 10
    done
    exit 1
    ;;
  -put)
    for attempt in 1 2 3 4 5 6; do
      if scp -q "${ssh_opts[@]}" -o ServerAliveInterval=15 "$2" "$host:$3"; then exit 0; fi
      echo "scp attempt $attempt failed, retrying" >&2
      sleep 10
    done
    exit 1
    ;;
  *)
    run_ps "$1"
    ;;
esac
