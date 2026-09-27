<#
.SYNOPSIS
Creates a mirror pool, fills it with pattern A, removes one disk, overwrites
the start of the space with pattern B while that disk is gone, then detaches
everything. The removed disk keeps stale copies that a reader must not use.

Pattern A uses the tag "<Name>", pattern B the tag "<Name>-new". The manifest
records which ranges hold which pattern.
#>
param(
    [Parameter(Mandatory)] [string] $Name,
    [int] $DiskCount = 3,
    [int] $DiskSizeMB = 8192,
    [int] $SizeMB = 1024,
    [int] $RewriteMB = 256,
    # -1: the disk holding copy 0 of the space's first extent.
    [int] $RemoveDisk = -1,
    [string] $Root = 'C:\sstest'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
if ($env:COMPUTERNAME -notin 'DESKTOP-BQ2J4NS', 'DESKTOP-ELS4LDK') { throw 'Unexpected machine' }

Add-Type -TypeDefinition @'
using System;
using System.IO;
public static class SsPattern {
    static ulong SplitMix(ref ulong s) {
        s += 0x9E3779B97F4A7C15UL;
        ulong z = s;
        z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9UL;
        z = (z ^ (z >> 27)) * 0x94D049BB133111EBUL;
        return z ^ (z >> 31);
    }
    public static void FillBlock(byte[] buf, int at, ulong offset, byte[] tag) {
        byte[] magic = System.Text.Encoding.ASCII.GetBytes("SSPATTRN");
        Buffer.BlockCopy(magic, 0, buf, at, 8);
        Buffer.BlockCopy(BitConverter.GetBytes(offset), 0, buf, at + 8, 8);
        Array.Clear(buf, at + 16, 16);
        Buffer.BlockCopy(tag, 0, buf, at + 16, Math.Min(tag.Length, 16));
        ulong s = offset;
        for (int i = 32; i < 4096; i += 8) {
            Buffer.BlockCopy(BitConverter.GetBytes(SplitMix(ref s)), 0, buf, at + i, 8);
        }
    }
    public static void Fill(string device, long size, string tag) { FillRange(device, 0, size, tag); }
    public static void FillRange(string device, long start, long size, string tag) {
        byte[] t = System.Text.Encoding.ASCII.GetBytes(tag);
        // 4 MiB writes are whole stripes for every layout we generate.
        const int chunk = 4 << 20;
        byte[] buf = new byte[chunk];
        using (var fs = new FileStream(device, FileMode.Open, FileAccess.ReadWrite, FileShare.ReadWrite, 4096, FileOptions.None)) {
            for (long pos = start; pos < start + size; pos += chunk) {
                int n = (int)Math.Min(chunk, start + size - pos);
                for (int b = 0; b < n; b += 4096) FillBlock(buf, b, (ulong)(pos + b), t);
                fs.Position = pos;
                fs.Write(buf, 0, n);
            }
            fs.Flush(true);
        }
    }
}
'@

$dir = Join-Path $Root $Name
if (Test-Path $dir) { throw "Test pool directory already exists: $dir" }
New-Item -ItemType Directory -Path $dir | Out-Null
$images = @()
for ($i = 0; $i -lt $DiskCount; $i++) {
    $f = Join-Path $dir ("disk{0}.vhdx" -f $i)
    $script = "create vdisk file=`"$f`" maximum=$DiskSizeMB type=expandable`r`nselect vdisk file=`"$f`"`r`nattach vdisk`r`n"
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
$vd = New-VirtualDisk -StoragePoolFriendlyName $poolName -FriendlyName $Name -ResiliencySettingName Mirror -NumberOfDataCopies 2 -NumberOfColumns 1 -ProvisioningType Fixed -Size ([int64]$SizeMB * 1MB)
$disk = $vd | Get-Disk
if ($disk.IsOffline) { $disk | Set-Disk -IsOffline $false }
if ($disk.IsReadOnly) { $disk | Set-Disk -IsReadOnly $false }
$vd = Get-VirtualDisk -FriendlyName $Name
$size = $vd.Size
$dev = "\\.\PhysicalDrive$($disk.Number)"
[SsPattern]::Fill($dev, $size, $Name)

$poolDisks = foreach ($f in $images) {
    $pd = Get-PhysicalDisk | Where-Object DeviceId -eq "$((Get-DiskImage -ImagePath $f).Number)"
    $guid = if ($pd.ObjectId -match 'PD:\{([0-9a-fA-F-]+)\}') { $Matches[1].ToLowerInvariant() } else { $null }
    [ordered]@{ image = Split-Path $f -Leaf; unique_id = $pd.UniqueId; spaces_guid = $guid; size = $pd.Size }
}

if ($RemoveDisk -lt 0) {
    $first = $vd | Get-PhysicalExtent | Where-Object { $_.VirtualDiskOffset -eq 0 -and $_.CopyNumber -eq 0 } | Sort-Object Size -Descending | Select-Object -First 1
    for ($i = 0; $i -lt $DiskCount; $i++) { if ($poolDisks[$i].unique_id -eq $first.PhysicalDiskUniqueId) { $RemoveDisk = $i } }
    if ($RemoveDisk -lt 0) { throw 'cannot find the disk holding copy 0' }
}
# Take one disk away and rewrite the start of the space.
Dismount-DiskImage -ImagePath $images[$RemoveDisk] | Out-Null
Start-Sleep -Seconds 5
[SsPattern]::FillRange($dev, 0, [int64]$RewriteMB * 1MB, "$Name-new")
Start-Sleep -Seconds 5
$vd = Get-VirtualDisk -FriendlyName $Name
$state = [ordered]@{ operational = "$($vd.OperationalStatus)"; health = "$($vd.HealthStatus)" }
$pool = Get-StoragePool -FriendlyName $poolName
$vdGuid = if ($vd.ObjectId -match '\{([0-9a-fA-F-]+)\}"?$') { $Matches[1].ToLowerInvariant() } else { $null }
$manifest = [ordered]@{
    name = $Name
    kind = 'stale'
    windows_build = [Environment]::OSVersion.Version.ToString()
    removed_disk = $RemoveDisk
    space = [ordered]@{ name = $Name; guid = $vdGuid; size = $size; state_while_degraded = $state }
    pool = [ordered]@{ health_while_degraded = "$($pool.HealthStatus)"; operational = "$($pool.OperationalStatus)" }
    disks = @($poolDisks)
    patterns = @(
        [ordered]@{ tag = "$Name-new"; start = 0; length = [int64]$RewriteMB * 1MB },
        [ordered]@{ tag = $Name; start = [int64]$RewriteMB * 1MB; length = $size - [int64]$RewriteMB * 1MB }
    )
}
$manifest | ConvertTo-Json -Depth 5 | Set-Content -Encoding UTF8 (Join-Path $dir 'manifest.json')
for ($i = 0; $i -lt $DiskCount; $i++) { if ($i -ne $RemoveDisk) { Dismount-DiskImage -ImagePath $images[$i] | Out-Null } }
"OK $dir"
