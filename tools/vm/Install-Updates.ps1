<#
.SYNOPSIS
Installs every available Windows update on a test VM (the VMs of older
Windows that make ReFS samples of other versions).

Windows Update refuses sessions logged on over the network (SSH, WinRM),
so the search, download and installation run as SYSTEM in a one-off
scheduled task. The script returns at once; the task writes its progress
to C:\sstest\logs\updates.log, ending with "EXIT 0" (and whether a restart
is needed), and is removed with -Cleanup. Run again after a restart until
it finds nothing: some updates come only after others.

With -PackageUrl it installs one package from the Microsoft Update
Catalog instead (an .msu on download.windowsupdate.com whose name ends in
the file's SHA-1; Windows Update stopped offering Server 2022 anything
after the 2023-11 cumulative update): downloaded, checked against that
SHA-1 and installed by wusa (DISM refuses .msu packages online), in the
same task and log.
#>
param([switch] $Cleanup, [string] $PackageUrl)
$ErrorActionPreference = 'Stop'
$task = 'sstest-windows-update'
if ($Cleanup) {
    Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
    'removed'
    return
}
New-Item -ItemType Directory -Force C:\sstest\logs | Out-Null
$inner = @'
$ErrorActionPreference = 'Continue'
"search $(Get-Date -Format s)"
$session = New-Object -ComObject Microsoft.Update.Session
$found = $session.CreateUpdateSearcher().Search("IsInstalled=0 and Type='Software' and IsHidden=0")
"found $($found.Updates.Count)"
if ($found.Updates.Count -gt 0) {
    $updates = New-Object -ComObject Microsoft.Update.UpdateColl
    foreach ($u in $found.Updates) {
        if (-not $u.EulaAccepted) { $u.AcceptEula() }
        [void]$updates.Add($u)
        "  $($u.Title)"
    }
    $downloader = $session.CreateUpdateDownloader()
    $downloader.Updates = $updates
    "download $($downloader.Download().ResultCode) $(Get-Date -Format s)"
    $installer = $session.CreateUpdateInstaller()
    $installer.Updates = $updates
    $result = $installer.Install()
    "install $($result.ResultCode) $(Get-Date -Format s), restart needed: $($result.RebootRequired)"
    for ($i = 0; $i -lt $updates.Count; $i++) { "  $($result.GetUpdateResult($i).ResultCode) $($updates.Item($i).Title)" }
}
'EXIT 0'
'@
if ($PackageUrl) {
    if ($PackageUrl -notmatch '^https://catalog\.s\.download\.windowsupdate\.com/.*_([0-9a-f]{40})\.msu$') { throw "not a catalog package: $PackageUrl" }
    $sha1 = $Matches[1]
    $inner = @"
`$ErrorActionPreference = 'Stop'
`$ProgressPreference = 'SilentlyContinue'
try {
    `$file = 'C:\sstest\update.msu'
    "download `$(Get-Date -Format s) $PackageUrl"
    Invoke-WebRequest -UseBasicParsing -Uri '$PackageUrl' -OutFile `$file
    `$hash = (Get-FileHash -Algorithm SHA1 `$file).Hash.ToLowerInvariant()
    if (`$hash -ne '$sha1') { throw "SHA-1 `$hash, not $sha1" }
    "install `$(Get-Date -Format s)"
    `$p = Start-Process -FilePath wusa.exe -ArgumentList `$file, '/quiet', '/norestart' -Wait -PassThru
    # 0 installed, 3010 installed (a restart needed), 2359302 already there.
    "wusa exit `$(`$p.ExitCode) `$(Get-Date -Format s)"
    Remove-Item `$file
    if (`$p.ExitCode -notin 0, 3010, 2359302) { throw "wusa failed: `$(`$p.ExitCode)" }
    'EXIT 0'
} catch { "ERROR: `$_"; 'EXIT 1' }
"@
}
$encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($inner))
$action = New-ScheduledTaskAction -Execute 'cmd.exe' -Argument "/c powershell.exe -NoProfile -NonInteractive -EncodedCommand $encoded > C:\sstest\logs\updates.log 2>&1"
Register-ScheduledTask -TaskName $task -Action $action -User 'SYSTEM' -RunLevel Highest -Force | Out-Null
Start-ScheduledTask -TaskName $task
'started; progress in C:\sstest\logs\updates.log'
