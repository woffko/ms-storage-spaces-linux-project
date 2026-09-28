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

Layout variations:
  -ExtraSpaces "name,resiliency,sizeMB[,columns];..."  more spaces in the pool,
      each filled with the pattern tagged with its own name (manifest
      "extra_spaces"); created before the main space, or after it with -ResizeMB
  -HoleMB N     a temporary space of N MB is created first and deleted before
      the main space, which then starts in the hole (fragmented allocation)
  -ResizeMB N   the main space is extended to N MB after its pattern was
      written, and the pattern is continued over the new part
  -Member4Kn    member VHDX files with 4096-byte logical sectors (the logical
      sector size item of the VHDX metadata is set before attaching)
  -SizeMB 0     the main space takes all remaining capacity (a full pool)
  -Finish       how the pool ends: Dismount detaches the VHDX files with the
      pool online (default; mirror dirty region logs end as after a Windows
      restart), Disconnect disconnects its spaces first (manual attach; empties
      the logs), ReadOnly sets the pool read-only first, Keep leaves it
      attached (for a restart)
  -IdleSeconds N  wait N seconds after the last write before finishing
  -Ntfs         instead of the pattern, the main space gets a GPT with one NTFS
      partition holding real files (System32 DLLs, -NtfsFilesMB in total) and
      random files of awkward sizes; the manifest lists every file with its
      SHA-256 ("files")
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
    [string] $ExtraSpaces = '',
    [int] $HoleMB = 0,
    [int] $ResizeMB = 0,
    [switch] $Member4Kn,
    [switch] $Ntfs,
    [int] $NtfsFilesMB = 512,
    [ValidateSet('Dismount', 'Disconnect', 'ReadOnly', 'Keep')] [string] $Finish = 'Dismount',
    [int] $IdleSeconds = 0,
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
    public static void Fill(string device, long size, string tag) { FillThrottled(device, size, tag, 0); }
    public static void FillThrottled(string device, long size, string tag, long maxBytesPerSecond) {
        FillRange(device, 0, size, tag, maxBytesPerSecond);
    }
    // maxBytesPerSecond > 0 writes through at a steady rate, so that a slow
    // host disk never builds up a long backlog (long stalls of the VM's
    // virtual NVMe controller crash Windows with 0x124).
    public static void FillRange(string device, long start, long end, string tag, long maxBytesPerSecond) {
        byte[] t = System.Text.Encoding.ASCII.GetBytes(tag);
        // 4 MiB writes are whole stripes for every layout we generate.
        const int chunk = 4 << 20;
        byte[] buf = new byte[chunk];
        var options = maxBytesPerSecond > 0 ? FileOptions.WriteThrough : FileOptions.None;
        var clock = System.Diagnostics.Stopwatch.StartNew();
        using (var fs = new FileStream(device, FileMode.Open, FileAccess.ReadWrite, FileShare.ReadWrite, 4096, options)) {
            for (long pos = start; pos < end; pos += chunk) {
                int n = (int)Math.Min(chunk, end - pos);
                for (int b = 0; b < n; b += 4096) FillBlock(buf, b, (ulong)(pos + b), t);
                fs.Position = pos;
                fs.Write(buf, 0, n);
                if (maxBytesPerSecond > 0) {
                    long due = (pos - start + n) * 1000 / maxBytesPerSecond - clock.ElapsedMilliseconds;
                    if (due > 0) System.Threading.Thread.Sleep((int)due);
                }
            }
            fs.Flush(true);
        }
    }
    // Sets the logical sector size item of a VHDX's metadata region (no
    // checksum covers it) to 4096; the file must not be attached.
    public static void SetLogicalSector4K(string path) {
        Guid metadataRegion = new Guid("8B7CA206-4790-4B9A-B8FE-575F050F886E");
        Guid logicalSector = new Guid("8141BF1D-A96F-4709-BA47-F233A8FAAB5F");
        using (var fs = new FileStream(path, FileMode.Open, FileAccess.ReadWrite)) {
            var r = new BinaryReader(fs);
            fs.Position = 0x30000;
            if (new string(r.ReadChars(4)) != "regi") throw new Exception("no VHDX region table");
            r.ReadUInt32();
            uint regions = r.ReadUInt32();
            r.ReadUInt32();
            long metadata = -1;
            for (uint i = 0; i < regions; i++) {
                var id = new Guid(r.ReadBytes(16));
                long offset = r.ReadInt64();
                r.ReadUInt32(); r.ReadUInt32();
                if (id == metadataRegion) metadata = offset;
            }
            if (metadata < 0) throw new Exception("no VHDX metadata region");
            fs.Position = metadata;
            if (new string(r.ReadChars(8)) != "metadata") throw new Exception("no VHDX metadata table");
            r.ReadUInt16();
            ushort entries = r.ReadUInt16();
            fs.Position = metadata + 32;
            for (int i = 0; i < entries; i++) {
                var id = new Guid(r.ReadBytes(16));
                uint offset = r.ReadUInt32();
                r.ReadUInt32(); r.ReadUInt32(); r.ReadUInt32();
                if (id == logicalSector) {
                    fs.Position = metadata + offset;
                    fs.Write(BitConverter.GetBytes(4096u), 0, 4);
                    return;
                }
            }
            throw new Exception("no logical sector size item");
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
    $scriptPath = Join-Path $dir 'diskpart.txt'
    [IO.File]::WriteAllText($scriptPath, "create vdisk file=`"$f`" maximum=$DiskSizeMB type=expandable`r`n")
    $out = diskpart /s $scriptPath
    if ($LASTEXITCODE -ne 0) { throw "diskpart failed: $out" }
    if ($Member4Kn) { [SsPattern]::SetLogicalSector4K($f) }
    [IO.File]::WriteAllText($scriptPath, "select vdisk file=`"$f`"`r`nattach vdisk`r`n")
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
    if ($SizeMB -gt 0) { $vdParams.Size = [int64]$SizeMB * 1MB } else { $vdParams.UseMaximumSize = $true }
}
if ($DataCopies -gt 0) { $vdParams.NumberOfDataCopies = $DataCopies }
if ($Redundancy -ge 0) { $vdParams.PhysicalDiskRedundancy = $Redundancy }
if ($Columns -gt 0) { $vdParams.NumberOfColumns = $Columns }
if ($InterleaveKB -gt 0) { $vdParams.Interleave = [int64]$InterleaveKB * 1KB }
if ($AllocationUnitMB -gt 0) { $vdParams.AllocationUnitSize = [int64]$AllocationUnitMB * 1MB }
if ($WriteCacheMB -ge 0) { $vdParams.WriteCacheSize = [int64]$WriteCacheMB * 1MB }
function Write-Pattern($vdName, [int64]$start, [int64]$end) {
    $d = Get-VirtualDisk -FriendlyName $vdName | Get-Disk
    if ($d.IsOffline) { $d | Set-Disk -IsOffline $false }
    if ($d.IsReadOnly) { $d | Set-Disk -IsReadOnly $false }
    [SsPattern]::FillRange("\\.\PhysicalDrive$($d.Number)", $start, $end, $vdName, [int64]$ThrottleMBps * 1MB)
}

$extra = @()
foreach ($spec in ($ExtraSpaces.Split(';') | Where-Object { $_ })) {
    $f = $spec.Split(',')
    $ep = @{
        StoragePoolFriendlyName = $poolName; FriendlyName = $f[0]; ResiliencySettingName = $f[1]
        Size = [int64]$f[2] * 1MB; ProvisioningType = 'Fixed'
    }
    if ($f.Count -gt 3) { $ep.NumberOfColumns = [int]$f[3] }
    $extra += , $ep
}
function New-ExtraSpaces {
    foreach ($ep in $extra) {
        New-VirtualDisk @ep | Out-Null
        if (-not $NoPattern) { Write-Pattern $ep.FriendlyName 0 (Get-VirtualDisk -FriendlyName $ep.FriendlyName).Size }
    }
}
if ($HoleMB -gt 0) {
    $hole = $vdParams.Clone()
    $hole.FriendlyName = "$Name-hole"
    $hole.Remove('UseMaximumSize')
    $hole.Size = [int64]$HoleMB * 1MB
    New-VirtualDisk @hole | Out-Null
}
if ($ResizeMB -le 0) { New-ExtraSpaces }
if ($HoleMB -gt 0) { Remove-VirtualDisk -FriendlyName "$Name-hole" -Confirm:$false }

$vd = New-VirtualDisk @vdParams
$disk = $vd | Get-Disk
if ($Ntfs) { $NoPattern = [switch]$true }
$patternSize = if ($NoPattern) { 0 } elseif ($PatternMB -gt 0) { [int64]$PatternMB * 1MB } else { $vd.Size }
if ($patternSize -gt 0) { Write-Pattern $Name 0 $patternSize }
if ($ResizeMB -gt 0) {
    # Other spaces take the next slabs, so the extension lands elsewhere.
    New-ExtraSpaces
    $vd | Resize-VirtualDisk -Size ([int64]$ResizeMB * 1MB)
    $vd = Get-VirtualDisk -FriendlyName $Name
    if (-not $NoPattern -and $PatternMB -le 0) {
        Write-Pattern $Name $patternSize $vd.Size
        $patternSize = $vd.Size
    }
}

$files = @()
if ($Ntfs) {
    if ($disk.IsOffline) { $disk | Set-Disk -IsOffline $false }
    if ($disk.IsReadOnly) { $disk | Set-Disk -IsReadOnly $false }
    Initialize-Disk -Number $disk.Number -PartitionStyle GPT
    $part = New-Partition -DiskNumber $disk.Number -UseMaximumSize -AssignDriveLetter
    Format-Volume -Partition $part -FileSystem NTFS -NewFileSystemLabel $Name -Confirm:$false | Out-Null
    $root = "$($part.DriveLetter):\"
    # Real files: System32 DLLs in name order up to the size limit.
    New-Item -ItemType Directory "$root\system32" | Out-Null
    $total = 0
    foreach ($f in Get-ChildItem C:\Windows\System32 -Filter *.dll -File | Sort-Object Name) {
        if ($total + $f.Length -gt [int64]$NtfsFilesMB * 1MB) { break }
        Copy-Item $f.FullName "$root\system32\"
        $total += $f.Length
    }
    # Random files around sector, cluster and extent boundaries, and an
    # empty file; a nested directory with a long Unicode name.
    $rng = New-Object Random 1234
    $dirName = "random - $([char]0x0444)$([char]0x0430)$([char]0x0439)$([char]0x043b)$([char]0x044b)"
    New-Item -ItemType Directory "$root\$dirName" | Out-Null
    foreach ($size in 0, 1, 511, 512, 4095, 4096, 4097, 65537, 1048593, 16777259, 67108879) {
        $b = New-Object byte[] $size
        $rng.NextBytes($b)
        [IO.File]::WriteAllBytes("$root\$dirName\file-$size.bin", $b)
    }
    Write-VolumeCache -DriveLetter $part.DriveLetter
    $files = @(Get-ChildItem $root -Recurse -File | Where-Object { $_.FullName -notlike '*System Volume Information*' } | ForEach-Object {
        [ordered]@{
            path = $_.FullName.Substring($root.Length).Replace('\', '/'); size = $_.Length
            sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash.ToLowerInvariant()
        }
    })
    Remove-PartitionAccessPath -DiskNumber $disk.Number -PartitionNumber $part.PartitionNumber -AccessPath $root
}

$poolDisks = foreach ($f in $images) {
    $number = (Get-DiskImage -ImagePath $f).Number
    $pd = Get-PhysicalDisk | Where-Object DeviceId -eq "$number"
    $guid = if ($pd.ObjectId -match 'PD:\{([0-9a-fA-F-]+)\}') { $Matches[1].ToLowerInvariant() } else { $null }
    [ordered]@{ image = Split-Path $f -Leaf; unique_id = $pd.UniqueId; spaces_guid = $guid; size = $pd.Size }
}
function Get-Extents($v) {
    @($v | Get-PhysicalExtent | ForEach-Object {
        [ordered]@{
            column = $_.ColumnNumber; copy = $_.CopyNumber; size = $_.Size
            virtual_offset = $_.VirtualDiskOffset; physical_offset = $_.PhysicalDiskOffset
            disk_unique_id = $_.PhysicalDiskUniqueId; status = "$($_.OperationalStatus)"
        }
    })
}
function Get-Guid($v) {
    # The ObjectId ends with "VD:{pool guid}{space guid}".
    if ($v.ObjectId -match '\{([0-9a-fA-F-]+)\}"?$') { $Matches[1].ToLowerInvariant() } else { $null }
}
$vd = Get-VirtualDisk -FriendlyName $Name
$extents = Get-Extents $vd
$pool = Get-StoragePool -FriendlyName $poolName
$vdGuid = Get-Guid $vd
$poolGuid = if ($pool.ObjectId -match 'SP:\{([0-9a-fA-F-]+)\}') { $Matches[1].ToLowerInvariant() } else { $null }
$manifest = [ordered]@{
    name = $Name
    windows_build = [Environment]::OSVersion.Version.ToString()
    pattern = -not $NoPattern
    pattern_size = $patternSize
    finish = $Finish
    idle_seconds = $IdleSeconds
    pool = [ordered]@{
        name = $poolName; guid = $poolGuid; version = "$($pool.Version)"; version_number = [int]$pool.CimInstanceProperties['Version'].Value; size = $pool.Size; allocated = $pool.AllocatedSize
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
    files = @($files)
    extra_spaces = @($extra | ForEach-Object {
        $v = Get-VirtualDisk -FriendlyName $_.FriendlyName
        [ordered]@{
            name = $v.FriendlyName; guid = Get-Guid $v; size = $v.Size
            resiliency = $v.ResiliencySettingName; copies = $v.NumberOfDataCopies
            redundancy = $v.PhysicalDiskRedundancy; columns = $v.NumberOfColumns
            interleave = $v.Interleave; provisioning = "$($v.ProvisioningType)"
            allocation_unit = $v.AllocationUnitSize
            pattern_size = if ($NoPattern) { 0 } else { $v.Size }
            extents = Get-Extents $v
        }
    })
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

if ($IdleSeconds -gt 0) { Start-Sleep -Seconds $IdleSeconds }
switch ($Finish) {
    'Disconnect' {
        # Only spaces that are attached manually can be disconnected.
        foreach ($v in Get-StoragePool -FriendlyName $poolName | Get-VirtualDisk) {
            $v | Set-VirtualDisk -IsManualAttach $true
            Disconnect-VirtualDisk -FriendlyName $v.FriendlyName
        }
    }
    'ReadOnly' { Set-StoragePool -FriendlyName $poolName -IsReadOnly $true }
}
# Detach so the VHDX files are consistent and can be copied.
if ($Finish -ne 'Keep') { foreach ($f in $images) { Dismount-DiskImage -ImagePath $f | Out-Null } }
Remove-Item (Join-Path $dir 'diskpart.txt') -ErrorAction SilentlyContinue
"OK $dir"
