<#
.SYNOPSIS
Deletes a test pool created by New-TestPool.ps1: attaches its VHDX files,
removes the space and the pool, detaches and deletes the files.
#>
param(
    [Parameter(Mandatory)] [string] $Name,
    [string] $Root = 'C:\sstest'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
if ($env:COMPUTERNAME -ne 'DESKTOP-ELS4LDK') { throw 'Unexpected machine' }
if ($Name -notmatch '^[A-Za-z0-9_-]+$') { throw 'Bad name' }

$dir = Join-Path $Root $Name
$images = @(Get-ChildItem -Path $dir -Filter 'disk*.vhdx' | ForEach-Object FullName)
foreach ($f in $images) {
    if (-not (Get-DiskImage -ImagePath $f).Attached) { Mount-DiskImage -ImagePath $f -NoDriveLetter | Out-Null }
}
Start-Sleep -Seconds 3
$pool = Get-StoragePool -FriendlyName "ss-$Name" -ErrorAction SilentlyContinue
if ($pool) {
    if ($pool.IsReadOnly) { $pool | Set-StoragePool -IsReadOnly $false }
    $pool | Get-VirtualDisk | Remove-VirtualDisk -Confirm:$false
    $pool | Remove-StoragePool -Confirm:$false
}
foreach ($f in $images) { Dismount-DiskImage -ImagePath $f | Out-Null }
Remove-Item -Recurse -Force $dir
"REMOVED $dir"
