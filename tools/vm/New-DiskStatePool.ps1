<#
.SYNOPSIS
Creates a mirror pool in a given disk or space state and records Windows'
view of it (manifest as New-TestPool.ps1, plus the states of the disks and
spaces).

Scenarios:
  Retired      the disk holding copy 0 is retired (Usage Retired) and the
               space repaired onto the other disks; the disk stays a member
  Interrupted  as Retired, but the member disks are detached a few seconds
               into the repair (copies still being regenerated); the manifest
               has no extent list, which was changing at that moment
  Replaced     a new disk is added, the old one retired, the space repaired and
               the old disk removed from the pool (its image is kept as
               removed0.vhdx and is not part of the manifest)
  SpaceStates  besides the main space, a space set to manual attach and
               detached, and a space whose disk is read-only
#>
param(
    [Parameter(Mandatory)] [string] $Name,
    [ValidateSet('Retired', 'Interrupted', 'Replaced', 'SpaceStates')] [string] $Scenario = 'Retired',
    [int] $DiskCount = 3,
    [int] $DiskSizeMB = 8192,
    [int] $SizeMB = 1024,
    [int] $ThrottleMBps = 30,
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
    public static void Fill(string device, long size, string tag, long maxBytesPerSecond) {
        byte[] t = System.Text.Encoding.ASCII.GetBytes(tag);
        const int chunk = 4 << 20;
        byte[] buf = new byte[chunk];
        var clock = System.Diagnostics.Stopwatch.StartNew();
        using (var fs = new FileStream(device, FileMode.Open, FileAccess.ReadWrite, FileShare.ReadWrite, 4096, FileOptions.WriteThrough)) {
            for (long pos = 0; pos < size; pos += chunk) {
                int n = (int)Math.Min(chunk, size - pos);
                for (int b = 0; b < n; b += 4096) FillBlock(buf, b, (ulong)(pos + b), t);
                fs.Position = pos;
                fs.Write(buf, 0, n);
                long due = (pos + n) * 1000 / maxBytesPerSecond - clock.ElapsedMilliseconds;
                if (due > 0) System.Threading.Thread.Sleep((int)due);
            }
            fs.Flush(true);
        }
    }
}
'@

function New-Vhdx($path) {
    $scriptPath = Join-Path $dir 'diskpart.txt'
    [IO.File]::WriteAllText($scriptPath, "create vdisk file=`"$path`" maximum=$DiskSizeMB type=expandable`r`nselect vdisk file=`"$path`"`r`nattach vdisk`r`n")
    $out = diskpart /s $scriptPath
    if ($LASTEXITCODE -ne 0) { throw "diskpart failed: $out" }
    Remove-Item $scriptPath
    Start-Sleep -Seconds 1
    Get-PhysicalDisk | Where-Object DeviceId -eq "$((Get-DiskImage -ImagePath $path).Number)"
}
function Get-Guid($v) {
    if ($v.ObjectId -match '\{([0-9a-fA-F-]+)\}"?$') { $Matches[1].ToLowerInvariant() } else { $null }
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
function Write-Pattern($vdName) {
    $v = Get-VirtualDisk -FriendlyName $vdName
    $d = $v | Get-Disk
    if ($d.IsOffline) { $d | Set-Disk -IsOffline $false }
    if ($d.IsReadOnly) { $d | Set-Disk -IsReadOnly $false }
    [SsPattern]::Fill("\\.\PhysicalDrive$($d.Number)", $v.Size, $vdName, [int64]$ThrottleMBps * 1MB)
}
function New-Space($vdName, [int]$mb) {
    New-VirtualDisk -StoragePoolFriendlyName $poolName -FriendlyName $vdName -ResiliencySettingName Mirror `
        -NumberOfDataCopies 2 -NumberOfColumns 1 -ProvisioningType Fixed -Size ([int64]$mb * 1MB) | Out-Null
    Write-Pattern $vdName
}
function Wait-Repair {
    while (Get-StorageJob | Where-Object { $_.JobState -eq 'Running' }) { Start-Sleep -Seconds 2 }
}

$dir = Join-Path $Root $Name
if (Test-Path $dir) { throw "Test pool directory already exists: $dir" }
New-Item -ItemType Directory -Path $dir | Out-Null
$images = @(for ($i = 0; $i -lt $DiskCount; $i++) { Join-Path $dir ("disk{0}.vhdx" -f $i) })
$physical = foreach ($f in $images) { New-Vhdx $f }
$subsystem = Get-StorageSubSystem | Where-Object FriendlyName -like 'Windows Storage*' | Select-Object -First 1
$poolName = "ss-$Name"
New-StoragePool -FriendlyName $poolName -StorageSubSystemUniqueId $subsystem.UniqueId -PhysicalDisks $physical | Out-Null
New-Space $Name $SizeMB
$vd = Get-VirtualDisk -FriendlyName $Name

# The member holding copy 0 of the start of the space.
$first = $vd | Get-PhysicalExtent | Where-Object { $_.VirtualDiskOffset -eq 0 -and $_.CopyNumber -eq 0 } | Select-Object -First 1
$victim = Get-PhysicalDisk -UniqueId $first.PhysicalDiskUniqueId
$extras = @()
$extentsKnown = $true
switch ($Scenario) {
    'Retired' {
        $victim | Set-PhysicalDisk -Usage Retired
        $vd | Repair-VirtualDisk
        Wait-Repair
    }
    'Interrupted' {
        $victim | Set-PhysicalDisk -Usage Retired
        $vd | Repair-VirtualDisk -AsJob | Out-Null
        Start-Sleep -Seconds 5
        $extentsKnown = $false
    }
    'Replaced' {
        $spare = Join-Path $dir 'spare.vhdx'
        Add-PhysicalDisk -StoragePoolFriendlyName $poolName -PhysicalDisks (New-Vhdx $spare)
        $images += $spare
        $victim | Set-PhysicalDisk -Usage Retired
        $vd | Repair-VirtualDisk
        Wait-Repair
        Remove-PhysicalDisk -StoragePoolFriendlyName $poolName -PhysicalDisks $victim -Confirm:$false
    }
    'SpaceStates' {
        New-Space "$Name-ma" 512
        Set-VirtualDisk -FriendlyName "$Name-ma" -IsManualAttach $true
        Disconnect-VirtualDisk -FriendlyName "$Name-ma" -Confirm:$false
        New-Space "$Name-ro" 512
        Get-VirtualDisk -FriendlyName "$Name-ro" | Get-Disk | Set-Disk -IsReadOnly $true
        $extras = @("$Name-ma", "$Name-ro")
    }
}

# Record the state before detaching. The victim of Replaced is no member
# any more; its image is renamed so that disk<N> are the members.
$vd = Get-VirtualDisk -FriendlyName $Name
$members = @($images | Where-Object {
    $n = (Get-DiskImage -ImagePath $_).Number
    $pd = Get-PhysicalDisk | Where-Object DeviceId -eq "$n"
    $pd -and ($pd | Get-StoragePool -ErrorAction SilentlyContinue | Where-Object FriendlyName -eq $poolName)
})
$disks = foreach ($f in $members) {
    $pd = Get-PhysicalDisk | Where-Object DeviceId -eq "$((Get-DiskImage -ImagePath $f).Number)"
    $guid = if ($pd.ObjectId -match 'PD:\{([0-9a-fA-F-]+)\}') { $Matches[1].ToLowerInvariant() } else { $null }
    [ordered]@{
        image = Split-Path $f -Leaf; unique_id = $pd.UniqueId; spaces_guid = $guid; size = $pd.Size
        usage = "$($pd.Usage)"; operational = "$($pd.OperationalStatus)"; health = "$($pd.HealthStatus)"
    }
}
$extents = if ($extentsKnown) { Get-Extents $vd } else { $null }
$pool = Get-StoragePool -FriendlyName $poolName
$poolGuid = if ($pool.ObjectId -match 'SP:\{([0-9a-fA-F-]+)\}') { $Matches[1].ToLowerInvariant() } else { $null }
function Get-SpaceState($v) {
    [ordered]@{
        operational = "$($v.OperationalStatus)"; health = "$($v.HealthStatus)"
        manual_attach = $v.IsManualAttach
        read_only = [bool]($v | Get-Disk -ErrorAction SilentlyContinue | ForEach-Object { $_.IsReadOnly })
    }
}
$manifest = [ordered]@{
    name = $Name
    kind = 'diskstate'
    scenario = $Scenario
    windows_build = [Environment]::OSVersion.Version.ToString()
    pattern = $true
    pattern_size = $vd.Size
    pool = [ordered]@{
        name = $poolName; guid = $poolGuid; version = "$($pool.Version)"
        version_number = [int]$pool.CimInstanceProperties['Version'].Value
        logical_sector = $pool.LogicalSectorSize; physical_sector = $pool.PhysicalSectorSize
    }
    space = [ordered]@{
        name = $Name; guid = Get-Guid $vd; size = $vd.Size
        resiliency = $vd.ResiliencySettingName; copies = $vd.NumberOfDataCopies
        redundancy = $vd.PhysicalDiskRedundancy; columns = $vd.NumberOfColumns
        interleave = $vd.Interleave; provisioning = "$($vd.ProvisioningType)"
        logical_sector = $vd.LogicalSectorSize; physical_sector = $vd.PhysicalSectorSize
        allocation_unit = $vd.AllocationUnitSize
        state = Get-SpaceState $vd
    }
    victim = $victim.UniqueId
    disks = @($disks)
    extents = $extents
    extra_spaces = @($extras | ForEach-Object {
        $v = Get-VirtualDisk -FriendlyName $_
        [ordered]@{
            name = $v.FriendlyName; guid = Get-Guid $v; size = $v.Size
            resiliency = $v.ResiliencySettingName; copies = $v.NumberOfDataCopies
            redundancy = $v.PhysicalDiskRedundancy; columns = $v.NumberOfColumns
            interleave = $v.Interleave; provisioning = "$($v.ProvisioningType)"
            allocation_unit = $v.AllocationUnitSize; pattern_size = $v.Size
            extents = Get-Extents $v; state = Get-SpaceState $v
        }
    })
}

foreach ($f in $images) { Dismount-DiskImage -ImagePath $f | Out-Null }
# Members become disk0..disk<N-1> in manifest order; others removed<K>.
$k = 0
foreach ($f in $images) {
    if ($members -notcontains $f) { Rename-Item $f ("removed{0}.vhdx" -f $k); $k++ }
}
for ($i = 0; $i -lt $members.Count; $i++) {
    $tmp = Join-Path $dir ("member{0}.tmp" -f $i)
    Rename-Item $members[$i] (Split-Path $tmp -Leaf)
}
for ($i = 0; $i -lt $members.Count; $i++) {
    Rename-Item (Join-Path $dir ("member{0}.tmp" -f $i)) ("disk{0}.vhdx" -f $i)
    $manifest.disks[$i].image = "disk$i.vhdx"
}
$manifest | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 (Join-Path $dir 'manifest.json')
"OK $dir"
