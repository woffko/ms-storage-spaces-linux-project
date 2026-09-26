<#
.SYNOPSIS
Creates a Storage Spaces test pool on VHDX files, fills the space with a
verifiable pattern, records Windows' view of the layout and detaches the disks.

Every 4096-byte block of the space at byte offset O contains:
  0..8    ASCII "SSPATTRN"
  8..16   O as little-endian u64
  16..32  the tag (space name, ASCII, zero padded)
  32..4096 splitmix64 stream seeded with O, little-endian u64 words
Run on the Windows test VM only.
#>
param(
    [Parameter(Mandatory)] [string] $Name,
    [int] $DiskCount = 2,
    [int] $DiskSizeMB = 8192,
    [ValidateSet('Simple', 'Mirror', 'Parity')] [string] $Resiliency = 'Simple',
    [int] $DataCopies = 0,
    [int] $Redundancy = -1,
    [int] $Columns = 0,
    [int] $InterleaveKB = 0,
    [ValidateSet('Thin', 'Fixed')] [string] $Provisioning = 'Fixed',
    [int] $SizeMB = 1024,
    [ValidateSet(0, 512, 4096)] [int] $LogicalSectorSize = 0,
    [int] $AllocationUnitMB = 0,
    [int] $WriteCacheMB = -1,
    # Steady write rate for the pattern (MB/s, 0 = unthrottled).
    [int] $ThrottleMBps = 15,
    # Tiered spaces: the first SsdDisks disks get media type SSD, the rest HDD;
    # Tiers lists "media,resiliency,sizeMB[,columns]" separated by ';'.
    [int] $SsdDisks = 0,
    [string] $Tiers = '',
    [switch] $NoPattern,
    [int] $PatternMB = 0,
    [string] $Root = 'C:\sstest'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

if ($env:COMPUTERNAME -ne 'DESKTOP-ELS4LDK') { throw 'Unexpected machine' }

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
    public static void Fill(string device, long size, string tag) { FillThrottled(device, size, tag, 0); }
    // maxBytesPerSecond > 0 writes through at a steady rate, so that a slow
    // host disk never builds up a long backlog (long stalls of the VM's
    // virtual NVMe controller crash Windows with 0x124).
    public static void FillThrottled(string device, long size, string tag, long maxBytesPerSecond) {
        byte[] t = System.Text.Encoding.ASCII.GetBytes(tag);
        // 4 MiB writes are whole stripes for every layout we generate.
        const int chunk = 4 << 20;
        byte[] buf = new byte[chunk];
        var options = maxBytesPerSecond > 0 ? FileOptions.WriteThrough : FileOptions.None;
        var clock = System.Diagnostics.Stopwatch.StartNew();
        using (var fs = new FileStream(device, FileMode.Open, FileAccess.ReadWrite, FileShare.ReadWrite, 4096, options)) {
            for (long pos = 0; pos < size; pos += chunk) {
                int n = (int)Math.Min(chunk, size - pos);
                for (int b = 0; b < n; b += 4096) FillBlock(buf, b, (ulong)(pos + b), t);
                fs.Position = pos;
                fs.Write(buf, 0, n);
                if (maxBytesPerSecond > 0) {
                    long due = (pos + n) * 1000 / maxBytesPerSecond - clock.ElapsedMilliseconds;
                    if (due > 0) System.Threading.Thread.Sleep((int)due);
                }
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
Start-Sleep -Seconds 2

$physical = foreach ($f in $images) {
    $number = (Get-DiskImage -ImagePath $f).Number
    Get-PhysicalDisk | Where-Object DeviceId -eq "$number"
}
$subsystem = Get-StorageSubSystem | Where-Object FriendlyName -like 'Windows Storage*' | Select-Object -First 1
$poolName = "ss-$Name"
$poolParams = @{ FriendlyName = $poolName; StorageSubSystemUniqueId = $subsystem.UniqueId; PhysicalDisks = $physical }
if ($LogicalSectorSize -gt 0) { $poolParams.LogicalSectorSizeDefault = $LogicalSectorSize }
New-StoragePool @poolParams | Out-Null

$vdParams = @{
    StoragePoolFriendlyName = $poolName
    FriendlyName            = $Name
    ProvisioningType        = $Provisioning
}
if ($Tiers) {
    $i = 0
    foreach ($pd in $physical) {
        $media = if ($i -lt $SsdDisks) { 'SSD' } else { 'HDD' }
        Get-PhysicalDisk -UniqueId $pd.UniqueId | Set-PhysicalDisk -MediaType $media
        $i++
    }
    $tierObjects = @(); $tierSizes = @()
    foreach ($spec in $Tiers.Split(';')) {
        $f = $spec.Split(',')
        $tp = @{
            StoragePoolFriendlyName = $poolName; FriendlyName = "$Name-$($f[0])"
            MediaType = $f[0]; ResiliencySettingName = $f[1]
        }
        if ($f.Count -gt 3) { $tp.NumberOfColumns = [int]$f[3] }
        $tierObjects += New-StorageTier @tp
        $tierSizes += [int64]$f[2] * 1MB
    }
    $vdParams.StorageTiers = $tierObjects
    $vdParams.StorageTierSizes = $tierSizes
} else {
    $vdParams.ResiliencySettingName = $Resiliency
    $vdParams.Size = [int64]$SizeMB * 1MB
}
if ($DataCopies -gt 0) { $vdParams.NumberOfDataCopies = $DataCopies }
if ($Redundancy -ge 0) { $vdParams.PhysicalDiskRedundancy = $Redundancy }
if ($Columns -gt 0) { $vdParams.NumberOfColumns = $Columns }
if ($InterleaveKB -gt 0) { $vdParams.Interleave = [int64]$InterleaveKB * 1KB }
if ($AllocationUnitMB -gt 0) { $vdParams.AllocationUnitSize = [int64]$AllocationUnitMB * 1MB }
if ($WriteCacheMB -ge 0) { $vdParams.WriteCacheSize = [int64]$WriteCacheMB * 1MB }
$vd = New-VirtualDisk @vdParams
$disk = $vd | Get-Disk

if (-not $NoPattern) {
    if ($disk.IsOffline) { $disk | Set-Disk -IsOffline $false }
    if ($disk.IsReadOnly) { $disk | Set-Disk -IsReadOnly $false }
    $patternSize = if ($PatternMB -gt 0) { [int64]$PatternMB * 1MB } else { $vd.Size }
    [SsPattern]::FillThrottled("\\.\PhysicalDrive$($disk.Number)", $patternSize, $Name, [int64]$ThrottleMBps * 1MB)
}

$poolDisks = foreach ($f in $images) {
    $number = (Get-DiskImage -ImagePath $f).Number
    $pd = Get-PhysicalDisk | Where-Object DeviceId -eq "$number"
    $guid = if ($pd.ObjectId -match 'PD:\{([0-9a-fA-F-]+)\}') { $Matches[1].ToLowerInvariant() } else { $null }
    [ordered]@{ image = Split-Path $f -Leaf; unique_id = $pd.UniqueId; spaces_guid = $guid; size = $pd.Size }
}
$vd = Get-VirtualDisk -FriendlyName $Name
$extents = $vd | Get-PhysicalExtent | ForEach-Object {
    [ordered]@{
        column = $_.ColumnNumber; copy = $_.CopyNumber; size = $_.Size
        virtual_offset = $_.VirtualDiskOffset; physical_offset = $_.PhysicalDiskOffset
        disk_unique_id = $_.PhysicalDiskUniqueId; status = "$($_.OperationalStatus)"
    }
}
$pool = Get-StoragePool -FriendlyName $poolName
# The ObjectId ends with "VD:{pool guid}{space guid}".
$vdGuid = if ($vd.ObjectId -match '\{([0-9a-fA-F-]+)\}"?$') { $Matches[1].ToLowerInvariant() } else { $null }
$poolGuid = if ($pool.ObjectId -match 'SP:\{([0-9a-fA-F-]+)\}') { $Matches[1].ToLowerInvariant() } else { $null }
$manifest = [ordered]@{
    name = $Name
    windows_build = [Environment]::OSVersion.Version.ToString()
    pattern = -not $NoPattern
    pattern_size = if ($NoPattern) { 0 } elseif ($PatternMB -gt 0) { [int64]$PatternMB * 1MB } else { $vd.Size }
    pool = [ordered]@{
        name = $poolName; guid = $poolGuid; version = "$($pool.Version)"; size = $pool.Size; allocated = $pool.AllocatedSize
        logical_sector = $pool.LogicalSectorSize; physical_sector = $pool.PhysicalSectorSize
    }
    space = [ordered]@{
        name = $Name; guid = $vdGuid; size = $vd.Size; footprint = $vd.FootprintOnPool
        resiliency = $vd.ResiliencySettingName; copies = $vd.NumberOfDataCopies
        redundancy = $vd.PhysicalDiskRedundancy; columns = $vd.NumberOfColumns
        interleave = $vd.Interleave; provisioning = "$($vd.ProvisioningType)"
        logical_sector = $vd.LogicalSectorSize; physical_sector = $vd.PhysicalSectorSize
        allocation_unit = $vd.AllocationUnitSize; groups = $vd.NumberOfGroups
        write_cache = $vd.WriteCacheSize; read_cache = $vd.ReadCacheSize
    }
    disks = @($poolDisks)
    extents = @($extents)
    tiers = @($vd | Get-StorageTier -ErrorAction SilentlyContinue | ForEach-Object {
        [ordered]@{
            name = $_.FriendlyName; media = "$($_.MediaType)"; size = $_.Size
            resiliency = $_.ResiliencySettingName; copies = $_.NumberOfDataCopies
            redundancy = $_.PhysicalDiskRedundancy; columns = $_.NumberOfColumns
            interleave = $_.Interleave
        }
    })
}
$manifest | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 (Join-Path $dir 'manifest.json')

# Detach so the VHDX files are consistent and can be copied.
foreach ($f in $images) { Dismount-DiskImage -ImagePath $f | Out-Null }
Remove-Item (Join-Path $dir 'diskpart.txt') -ErrorAction SilentlyContinue
"OK $dir"
