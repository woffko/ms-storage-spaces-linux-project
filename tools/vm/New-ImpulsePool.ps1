<#
.SYNOPSIS
Creates a dual parity space without write-back cache and writes "impulse"
stripes: all-zero stripes where single bytes of chosen data units are set.
Reading the second parity unit (Q) of each stripe shows how Windows computes
it. The manifest lists the impulses per stripe.

Dual parity spaces of 11 and more columns use a local reconstruction code
with fewer data columns (-DataColumns, e.g. 9 for 12 columns) and always get
a write-back cache; -FlushMB then writes that many MB of zeros after the
impulses, so that the impulse stripes are moved out of the cache to their
parity stripes before the pool is detached.
#>
param(
    [Parameter(Mandatory)] [string] $Name,
    [int] $DiskCount = 0,
    [int] $Columns = 7,
    [int] $DataColumns = 0,
    [int] $FlushMB = 0,
    [int] $InterleaveKB = 64,
    [int] $SizeMB = 5120,
    [switch] $Simple,
    [string] $Root = 'C:\sstest'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
if ($env:COMPUTERNAME -notin 'DESKTOP-BQ2J4NS', 'DESKTOP-ELS4LDK') { throw 'Unexpected machine' }

# stripe -> list of (data unit index, byte offset in the unit, value).
# -Simple: stripe 0 empty, then one stripe per data unit with byte 0 = 1.
if ($Simple) {
    $impulses = @(, @())
    for ($k = 0; $k -lt $(if ($DataColumns -gt 0) { $DataColumns } else { $Columns - 2 }); $k++) { $impulses += , @(, @($k, 0, 1)) }
    $impulses += , @()
} else {
    $impulses = @(
        @(),
        @(, @(0, 0, 1)),
        @(, @(0, 0, 2)),
        @(, @(0, 1, 1)),
        @(, @(1, 0, 1)),
        @(@(0, 0, 1), @(1, 0, 1)),
        @(, @(0, 4096, 1)),
        @(, @(2, 0, 1)),
        @(, @(3, 0, 1)),
        @(, @(4, 0, 1)),
        @(, @(0, 0, 0x80)),
        @(, @(0, 512, 1)),
        @(, @(0, 8, 1)),
        @(, @(0, 0, 3)),
        @()
    )
}
if ($DiskCount -le 0) { $DiskCount = $Columns }
$dir = Join-Path $Root $Name
if (Test-Path $dir) { throw "Test pool directory already exists: $dir" }
New-Item -ItemType Directory -Path $dir | Out-Null
$images = @()
for ($i = 0; $i -lt $DiskCount; $i++) {
    $f = Join-Path $dir ("disk{0}.vhdx" -f $i)
    $script = "create vdisk file=`"$f`" maximum=8192 type=expandable`r`nselect vdisk file=`"$f`"`r`nattach vdisk`r`n"
    $scriptPath = Join-Path $dir 'diskpart.txt'
    [IO.File]::WriteAllText($scriptPath, $script)
    $out = diskpart /s $scriptPath
    if ($LASTEXITCODE -ne 0) { throw "diskpart failed: $out" }
    $images += $f
}
Remove-Item (Join-Path $dir 'diskpart.txt')
Start-Sleep -Seconds 2
$physical = foreach ($f in $images) { Get-PhysicalDisk | Where-Object DeviceId -eq "$((Get-DiskImage -ImagePath $f).Number)" }
$subsystem = Get-StorageSubSystem | Where-Object FriendlyName -like 'Windows Storage*' | Select-Object -First 1
$poolName = "ss-$Name"
New-StoragePool -FriendlyName $poolName -StorageSubSystemUniqueId $subsystem.UniqueId -PhysicalDisks $physical | Out-Null
$vd = New-VirtualDisk -StoragePoolFriendlyName $poolName -FriendlyName $Name -ResiliencySettingName Parity `
    -PhysicalDiskRedundancy 2 -NumberOfColumns $Columns -Interleave ([int64]$InterleaveKB * 1KB) `
    -ProvisioningType Fixed -Size ([int64]$SizeMB * 1MB) -WriteCacheSize 0
$disk = $vd | Get-Disk
if ($disk.IsOffline) { $disk | Set-Disk -IsOffline $false }
if ($disk.IsReadOnly) { $disk | Set-Disk -IsReadOnly $false }
$vd = Get-VirtualDisk -FriendlyName $Name

$unit = [int64]$InterleaveKB * 1KB
$dataColumns = if ($DataColumns -gt 0) { $DataColumns } else { $Columns - 2 }
$stripeBytes = $unit * $dataColumns
Add-Type -TypeDefinition @'
using System;
using System.IO;
public static class SsImpulse {
    public static void WriteAt(string device, long offset, byte[] data) {
        using (var fs = new FileStream(device, FileMode.Open, FileAccess.ReadWrite, FileShare.ReadWrite, 4096, FileOptions.WriteThrough)) {
            fs.Position = offset;
            fs.Write(data, 0, data.Length);
            fs.Flush(true);
        }
    }
}
'@
# Wait until the new space accepts I/O.
$device = "\\.\PhysicalDrive$($disk.Number)"
for ($try = 0; $try -lt 30; $try++) {
    try { [SsImpulse]::WriteAt($device, 0, (New-Object byte[] 4096)); break } catch { Start-Sleep -Seconds 2 }
}
for ($s = 0; $s -lt $impulses.Count; $s++) {
    $buf = New-Object byte[] $stripeBytes
    foreach ($imp in $impulses[$s]) {
        if ($imp.Count -lt 3) { continue }
        $buf[[int64]$imp[0] * $unit + $imp[1]] = [byte]$imp[2]
    }
    [SsImpulse]::WriteAt($device, $s * $stripeBytes, $buf)
}
if ($FlushMB -gt 0) {
    # 4 MiB write-through writes, paced at about 15 MB/s (see New-TestPool.ps1).
    $zeros = New-Object byte[] (4MB)
    $at = [int64]$impulses.Count * $stripeBytes
    $at = [int64][Math]::Ceiling($at / 4MB) * 4MB
    for ($i = 0; $i -lt $FlushMB / 4; $i++) {
        [SsImpulse]::WriteAt($device, $at + [int64]$i * 4MB, $zeros)
        Start-Sleep -Milliseconds 270
    }
}
Start-Sleep -Seconds 5

$vdGuid = if ($vd.ObjectId -match '\{([0-9a-fA-F-]+)\}"?$') { $Matches[1].ToLowerInvariant() } else { $null }
$poolDisks = foreach ($f in $images) {
    $pd = Get-PhysicalDisk | Where-Object DeviceId -eq "$((Get-DiskImage -ImagePath $f).Number)"
    $guid = if ($pd.ObjectId -match 'PD:\{([0-9a-fA-F-]+)\}') { $Matches[1].ToLowerInvariant() } else { $null }
    [ordered]@{ image = Split-Path $f -Leaf; unique_id = $pd.UniqueId; spaces_guid = $guid; size = $pd.Size }
}
$manifest = [ordered]@{
    name = $Name
    kind = 'impulse'
    windows_build = [Environment]::OSVersion.Version.ToString()
    space = [ordered]@{ name = $Name; guid = $vdGuid; size = $vd.Size; columns = $vd.NumberOfColumns
        interleave = $vd.Interleave; redundancy = $vd.PhysicalDiskRedundancy; write_cache = $vd.WriteCacheSize
        data_columns = $dataColumns; stripe_size = $stripeBytes }
    disks = @($poolDisks)
    impulses = @($impulses | ForEach-Object { , @($_ | Where-Object { $_.Count -ge 3 } | ForEach-Object { , @($_) }) })
}
$manifest | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 (Join-Path $dir 'manifest.json')
foreach ($f in $images) { Dismount-DiskImage -ImagePath $f | Out-Null }
"OK $dir"
