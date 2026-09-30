<#
.SYNOPSIS
Runs a scripted scenario on an attached test pool and takes snapshots of its
member disks between the steps (for the experiments of M5 in docs/plan.md).

The pool is made by New-TestPool.ps1 -Finish Keep, so it stays attached, or
by the steps blank and newpool (the directory is created then).
-Steps lists operations separated by ';', arguments separated by ':':
  blank:COUNT[:SIZEMB[:4kn]]  create COUNT more VHDX files (default 8 GiB,
                             4kn: 4 KiB logical sectors) and attach them
                             without putting them in a pool
  newpool[:SECTOR]           New-StoragePool (ss-<Name>) of every attached
                             image; SECTOR sets LogicalSectorSizeDefault;
                             writes manifest.json (pool and disks)
  newspacex:NAME:K=V,...     New-VirtualDisk with res (Simple, Mirror,
                             Parity), size (MB), prov (Thin, Fixed), cols,
                             copies, red, il (KB), au (MB), wc (write cache
                             MB)
  renamepool:NEW             rename the pool (later steps use the new name)
  media:I:TYPE               Set-PhysicalDisk -MediaType (HDD, SSD) on I
  usage:I:USAGE              Set-PhysicalDisk -Usage on member I
  optimize                   Optimize-StoragePool, then wait for its jobs
  waitjobs                   wait until no storage job runs
  removepool                 Remove-StoragePool (its spaces removed first)
  snap:LABEL[:MB]            snapshot every attached member disk to
                             C:\sstest\<Name>\snap-LABEL\disk<i>.snap, plus
                             state.json (health, extents, disks); with MB only
                             the first MB MiB are read (a few seconds instead of
                             a minute; the rest of the image stays zero)
  write:SPACE:OFFKB:LENKB:TAG  write the verification pattern tagged TAG
                             (write-through, flushed)
  writes:SPACE:COUNT:STRIDEKB:LENKB:TAG  COUNT writes of LENKB, one every
                             STRIDEKB from 0 (each flushed)
  sleep:SECONDS
  rename:SPACE:NEWNAME
  resize:SPACE:SIZEMB
  disconnect:SPACE           set manual attach and disconnect the space
  connect:SPACE
  detachdisk:I / attachdisk:I  dismount / mount member image disk<I>.vhdx
  newdisk:I                  create disk<I>.vhdx (8 GiB) and add it to the pool
  retire:I                   set the usage of member I to Retired
  removedisk:I               remove member I from the pool
  repair:SPACE               Repair-VirtualDisk (waits for it)
  readonly:true|false        set the pool read-only or writable
  newspace:NAME:RESILIENCY:SIZEMB[:Thin|Fixed]
  removespace:NAME
  format:SPACE               GPT, one partition and NTFS on the space
  file:SPACE:NAME:MB         write a file of MB MiB (random data) there
  delfile:SPACE:NAME         delete it
  retrim:SPACE               Optimize-Volume -ReTrim (TRIM of free space)
  dismount                   detach every member image
A snapshot file ("SSSNAP01") holds the disk size and runs of 4 KiB pages:
kind 1 = data (offset u64, pages u32, the pages), kind 2 = verification
pattern (offset u64, pages u32, pattern offset u64, tag[16]); zero pages
are left out. `spaces snapshot-to-raw` turns it into a raw image.
Run on the Windows test VM only.
#>
param(
    [Parameter(Mandatory)] [string] $Name,
    [Parameter(Mandatory)] [string] $Steps,
    [string] $Root = 'C:\sstest'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
if ($env:COMPUTERNAME -notin 'DESKTOP-BQ2J4NS', 'DESKTOP-ELS4LDK') { throw 'Unexpected machine' }
if ($Name -notmatch '^[A-Za-z0-9_-]+$') { throw 'Bad name' }

Add-Type -TypeDefinition @'
using System;
using System.IO;
public static class SsScenario {
    static ulong SplitMix(ref ulong s) {
        s += 0x9E3779B97F4A7C15UL;
        ulong z = s;
        z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9UL;
        z = (z ^ (z >> 27)) * 0x94D049BB133111EBUL;
        return z ^ (z >> 31);
    }
    static readonly byte[] Magic = System.Text.Encoding.ASCII.GetBytes("SSPATTRN");
    static void FillBlock(byte[] buf, int at, ulong offset, byte[] tag, int tagAt) {
        Buffer.BlockCopy(Magic, 0, buf, at, 8);
        Buffer.BlockCopy(BitConverter.GetBytes(offset), 0, buf, at + 8, 8);
        Buffer.BlockCopy(tag, tagAt, buf, at + 16, 16);
        ulong s = offset;
        for (int i = 32; i < 4096; i += 8) Buffer.BlockCopy(BitConverter.GetBytes(SplitMix(ref s)), 0, buf, at + i, 8);
    }
    public static void Write(string device, long start, long end, string tag) {
        byte[] t = new byte[16];
        byte[] a = System.Text.Encoding.ASCII.GetBytes(tag);
        Buffer.BlockCopy(a, 0, t, 0, Math.Min(a.Length, 16));
        const int chunk = 1 << 20;
        byte[] buf = new byte[chunk];
        using (var fs = new FileStream(device, FileMode.Open, FileAccess.ReadWrite, FileShare.ReadWrite, 4096, FileOptions.WriteThrough)) {
            for (long pos = start; pos < end; pos += chunk) {
                int n = (int)Math.Min(chunk, end - pos);
                for (int b = 0; b < n; b += 4096) FillBlock(buf, b, (ulong)(pos + b), t, 0);
                fs.Position = pos;
                fs.Write(buf, 0, n);
            }
            fs.Flush(true);
        }
    }
    static bool IsZero(byte[] b, int at) {
        for (int i = at; i < at + 4096; i += 8) if (BitConverter.ToUInt64(b, i) != 0) return false;
        return true;
    }
    // Snapshot of a disk (see the script help for the format).
    public static long[] Snapshot(string device, long size, long limit, string path) {
        byte[] buf = new byte[1 << 20];
        byte[] expected = new byte[4096];
        long dataPages = 0, patternPages = 0;
        using (var src = new FileStream(device, FileMode.Open, FileAccess.Read, FileShare.ReadWrite, 4096))
        using (var dst = new BinaryWriter(File.Create(path))) {
            dst.Write(System.Text.Encoding.ASCII.GetBytes("SSSNAP01"));
            dst.Write(size);
            // The run being collected: kind 0 = none.
            int kind = 0; long start = 0; int pages = 0; ulong patternStart = 0; byte[] tag = new byte[16];
            var data = new MemoryStream();
            Action flush = () => {
                if (kind == 1) { dst.Write((byte)1); dst.Write(start); dst.Write(pages); dst.Flush(); data.WriteTo(dst.BaseStream); }
                if (kind == 2) { dst.Write((byte)2); dst.Write(start); dst.Write(pages); dst.Write(patternStart); dst.Write(tag); }
                kind = 0; pages = 0; data.SetLength(0);
            };
            for (long pos = 0; pos < limit; pos += buf.Length) {
                int n = (int)Math.Min(buf.Length, limit - pos);
                src.Position = pos;
                for (int got = 0; got < n; ) {
                    int r = src.Read(buf, got, n - got);
                    if (r <= 0) throw new IOException("short read at " + (pos + got));
                    got += r;
                }
                for (int p = 0; p < n; p += 4096) {
                    long at = pos + p;
                    if (IsZero(buf, p)) { flush(); continue; }
                    bool pattern = false;
                    ulong po = BitConverter.ToUInt64(buf, p + 8);
                    if (BitConverter.ToUInt64(buf, p) == BitConverter.ToUInt64(Magic, 0) && po % 4096 == 0) {
                        FillBlock(expected, 0, po, buf, p + 16);
                        pattern = true;
                        for (int i = 0; i < 4096 && pattern; i += 8)
                            if (BitConverter.ToUInt64(buf, p + i) != BitConverter.ToUInt64(expected, i)) pattern = false;
                    }
                    if (pattern) {
                        bool sameTag = true;
                        for (int i = 0; i < 16; i++) if (tag[i] != buf[p + 16 + i]) sameTag = false;
                        if (!(kind == 2 && start + pages * 4096L == at && patternStart + (ulong)pages * 4096UL == po && sameTag)) {
                            flush();
                            kind = 2; start = at; patternStart = po;
                            Buffer.BlockCopy(buf, p + 16, tag, 0, 16);
                        }
                        pages++; patternPages++;
                    } else {
                        if (!(kind == 1 && start + pages * 4096L == at)) { flush(); kind = 1; start = at; }
                        data.Write(buf, p, 4096);
                        pages++; dataPages++;
                    }
                }
            }
            flush();
        }
        return new long[] { dataPages, patternPages };
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
$poolName = "ss-$Name"
if (-not (Test-Path $dir)) {
    if (-not $Steps.StartsWith('blank:')) { throw "no test pool $dir" }
    New-Item -ItemType Directory -Path $dir | Out-Null
}
function Get-MemberDisk([int] $i) {
    $number = (Get-DiskImage -ImagePath (Get-Image $i)).Number
    Get-PhysicalDisk | Where-Object DeviceId -eq "$number"
}
function Wait-Jobs {
    do {
        Start-Sleep -Seconds 5
        $running = @(Get-StorageJob | Where-Object { $_.JobState -eq 'Running' -or $_.JobState -eq 'New' })
    } while ($running.Count -gt 0)
}
function Get-Image([int] $i) { Join-Path $dir ("disk{0}.vhdx" -f $i) }
function Get-Images { @(Get-ChildItem $dir -Filter 'disk*.vhdx' | Sort-Object { [int]($_.BaseName -replace '\D', '') } | ForEach-Object FullName) }
function Get-SpaceDevice([string] $space) {
    $d = Get-VirtualDisk -FriendlyName $space | Get-Disk
    if ($d.IsOffline) { $d | Set-Disk -IsOffline $false }
    if ($d.IsReadOnly) { $d | Set-Disk -IsReadOnly $false }
    "\\.\PhysicalDrive$($d.Number)"
}
function Get-SpaceVolume([string] $space) {
    $d = Get-VirtualDisk -FriendlyName $space | Get-Disk
    $part = $d | Get-Partition | Where-Object Type -eq 'Basic' | Select-Object -First 1
    if (-not $part.DriveLetter) {
        $part | Add-PartitionAccessPath -AssignDriveLetter
        $part = Get-Partition -DiskNumber $d.Number -PartitionNumber $part.PartitionNumber
    }
    "$($part.DriveLetter):"
}
function Get-State {
    $p = Get-StoragePool -FriendlyName $poolName -ErrorAction SilentlyContinue
    if (-not $p) { return [ordered]@{ pool = $null } }
    [ordered]@{
        pool   = [ordered]@{ health = "$($p.HealthStatus)"; operational = "$($p.OperationalStatus)"; read_only = $p.IsReadOnly }
        spaces = @($p | Get-VirtualDisk | ForEach-Object {
            $v = $_
            [ordered]@{
                name = $v.FriendlyName; health = "$($v.HealthStatus)"; operational = "$($v.OperationalStatus)"
                size = $v.Size; footprint = $v.FootprintOnPool; manual_attach = $v.IsManualAttach
                extents = @(if ($v.OperationalStatus -ne 'Detached') { $v | Get-PhysicalExtent | ForEach-Object {
                    [ordered]@{
                        column = $_.ColumnNumber; copy = $_.CopyNumber; size = $_.Size
                        virtual_offset = $_.VirtualDiskOffset; physical_offset = $_.PhysicalDiskOffset
                        disk_unique_id = $_.PhysicalDiskUniqueId; status = "$($_.OperationalStatus)"
                    }
                } })
            }
        })
        disks  = @($p | Get-PhysicalDisk | ForEach-Object {
            [ordered]@{
                unique_id = $_.UniqueId; health = "$($_.HealthStatus)"; operational = "$($_.OperationalStatus)"
                usage = "$($_.Usage)"; allocated = $_.AllocatedSize; size = $_.Size
            }
        })
    }
}

$log = @()
foreach ($step in ($Steps.Split(';') | Where-Object { $_ })) {
    $a = $step.Split(':')
    $started = Get-Date
    switch ($a[0]) {
        'snap' {
            $out = Join-Path $dir "snap-$($a[1])"
            if (Test-Path $out) { throw "snapshot $($a[1]) exists" }
            New-Item -ItemType Directory $out | Out-Null
            $images = @(Get-Images)
            for ($i = 0; $i -lt $images.Count; $i++) {
                $img = Get-DiskImage -ImagePath $images[$i]
                if (-not $img.Attached) { continue }
                $limit = if ($a.Count -gt 2) { [Math]::Min($img.Size, [int64]$a[2] * 1MB) } else { $img.Size }
                # Right after New-StoragePool a member can be away for a moment.
                for ($try = 1; ; $try++) {
                    try {
                        $pages = [SsScenario]::Snapshot("\\.\PhysicalDrive$((Get-DiskImage -ImagePath $images[$i]).Number)", $img.Size, $limit, (Join-Path $out "disk$i.snap"))
                        break
                    } catch {
                        if ($try -ge 10) { throw }
                        Start-Sleep -Seconds 3
                    }
                }
                "  disk$i`: $($pages[0]) data pages, $($pages[1]) pattern pages"
            }
            Get-State | ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 (Join-Path $out 'state.json')
        }
        'write' {
            $dev = Get-SpaceDevice $a[1]
            [SsScenario]::Write($dev, [int64]$a[2] * 1KB, ([int64]$a[2] + [int64]$a[3]) * 1KB, $a[4])
        }
        'writes' {
            $dev = Get-SpaceDevice $a[1]
            for ($k = 0; $k -lt [int]$a[2]; $k++) {
                $at = [int64]$k * [int64]$a[3] * 1KB
                [SsScenario]::Write($dev, $at, $at + [int64]$a[4] * 1KB, $a[5])
            }
        }
        'sleep' { Start-Sleep -Seconds ([int]$a[1]) }
        'rename' { Set-VirtualDisk -FriendlyName $a[1] -NewFriendlyName $a[2] }
        'resize' { Get-VirtualDisk -FriendlyName $a[1] | Resize-VirtualDisk -Size ([int64]$a[2] * 1MB) }
        'disconnect' {
            Set-VirtualDisk -FriendlyName $a[1] -IsManualAttach $true
            Disconnect-VirtualDisk -FriendlyName $a[1]
        }
        'connect' { Connect-VirtualDisk -FriendlyName $a[1] }
        'detachdisk' { Dismount-DiskImage -ImagePath (Get-Image $a[1]) | Out-Null }
        'attachdisk' { Mount-DiskImage -ImagePath (Get-Image $a[1]) | Out-Null; Start-Sleep -Seconds 3 }
        'newdisk' {
            $f = Get-Image $a[1]
            if (Test-Path $f) { throw "$f exists" }
            $scriptPath = Join-Path $dir 'diskpart.txt'
            [IO.File]::WriteAllText($scriptPath, "create vdisk file=`"$f`" maximum=8192 type=expandable`r`nselect vdisk file=`"$f`"`r`nattach vdisk`r`n")
            $out = diskpart /s $scriptPath
            if ($LASTEXITCODE -ne 0) { throw "diskpart failed: $out" }
            Remove-Item $scriptPath
            Start-Sleep -Seconds 2
            $number = (Get-DiskImage -ImagePath $f).Number
            $pd = Get-PhysicalDisk | Where-Object DeviceId -eq "$number"
            Add-PhysicalDisk -StoragePoolFriendlyName $poolName -PhysicalDisks $pd
        }
        'retire' { Get-MemberDisk $a[1] | Set-PhysicalDisk -Usage Retired }
        'removedisk' {
            $number = (Get-DiskImage -ImagePath (Get-Image $a[1])).Number
            $pd = Get-PhysicalDisk | Where-Object DeviceId -eq "$number"
            Remove-PhysicalDisk -StoragePoolFriendlyName $poolName -PhysicalDisks $pd -Confirm:$false
        }
        'repair' { Repair-VirtualDisk -FriendlyName $a[1] }
        'readonly' { Set-StoragePool -FriendlyName $poolName -IsReadOnly ([bool]::Parse($a[1])) }
        'newspace' {
            $p = @{ StoragePoolFriendlyName = $poolName; FriendlyName = $a[1]; ResiliencySettingName = $a[2]; Size = [int64]$a[3] * 1MB }
            if ($a.Count -gt 4) { $p.ProvisioningType = $a[4] }
            New-VirtualDisk @p | Out-Null
        }
        'removespace' { Remove-VirtualDisk -FriendlyName $a[1] -Confirm:$false }
        'blank' {
            $first = @(Get-Images).Count
            $sizeMB = if ($a.Count -gt 2) { [int]$a[2] } else { 8192 }
            for ($i = $first; $i -lt $first + [int]$a[1]; $i++) {
                $f = Get-Image $i
                $scriptPath = Join-Path $dir 'diskpart.txt'
                [IO.File]::WriteAllText($scriptPath, "create vdisk file=`"$f`" maximum=$sizeMB type=expandable`r`n")
                $out = diskpart /s $scriptPath
                if ($LASTEXITCODE -ne 0) { throw "diskpart failed: $out" }
                if ($a.Count -gt 3 -and $a[3] -eq '4kn') { [SsScenario]::SetLogicalSector4K($f) }
                [IO.File]::WriteAllText($scriptPath, "select vdisk file=`"$f`"`r`nattach vdisk`r`n")
                $out = diskpart /s $scriptPath
                if ($LASTEXITCODE -ne 0) { throw "diskpart failed: $out" }
                Remove-Item $scriptPath
            }
            Start-Sleep -Seconds 2
        }
        'newpool' {
            $physical = @(for ($i = 0; $i -lt @(Get-Images).Count; $i++) { Get-MemberDisk $i })
            $subsystem = Get-StorageSubSystem | Where-Object FriendlyName -like 'Windows Storage*' | Select-Object -First 1
            $pp = @{ FriendlyName = $poolName; StorageSubSystemUniqueId = $subsystem.UniqueId; PhysicalDisks = $physical }
            if ($a.Count -gt 1) { $pp.LogicalSectorSizeDefault = [int]$a[1] }
            New-StoragePool @pp | Out-Null
            $pool = Get-StoragePool -FriendlyName $poolName
            [ordered]@{
                name = $Name
                windows_build = [Environment]::OSVersion.Version.ToString()
                pattern = $false
                pool = [ordered]@{
                    name = $poolName
                    guid = if ($pool.ObjectId -match 'SP:\{([0-9a-fA-F-]+)\}') { $Matches[1].ToLowerInvariant() } else { $null }
                    version = "$($pool.Version)"; version_number = [int]$pool.CimInstanceProperties['Version'].Value
                    size = $pool.Size; allocated = $pool.AllocatedSize
                    logical_sector = $pool.LogicalSectorSize; physical_sector = $pool.PhysicalSectorSize
                }
                disks = @(for ($i = 0; $i -lt @(Get-Images).Count; $i++) {
                    $pd = Get-MemberDisk $i
                    [ordered]@{
                        image = Split-Path (Get-Image $i) -Leaf; unique_id = $pd.UniqueId; size = $pd.Size
                        spaces_guid = if ($pd.ObjectId -match 'PD:\{([0-9a-fA-F-]+)\}') { $Matches[1].ToLowerInvariant() } else { $null }
                    }
                })
            } | ConvertTo-Json -Depth 5 | Set-Content -Encoding UTF8 (Join-Path $dir 'manifest.json')
        }
        'newspacex' {
            $p = @{ StoragePoolFriendlyName = $poolName; FriendlyName = $a[1] }
            foreach ($kv in $a[2].Split(',')) {
                $k, $v = $kv.Split('=')
                switch ($k) {
                    'res' { $p.ResiliencySettingName = $v }
                    'size' { $p.Size = [int64]$v * 1MB }
                    'prov' { $p.ProvisioningType = $v }
                    'cols' { $p.NumberOfColumns = [int]$v }
                    'copies' { $p.NumberOfDataCopies = [int]$v }
                    'red' { $p.PhysicalDiskRedundancy = [int]$v }
                    'il' { $p.Interleave = [int64]$v * 1KB }
                    'au' { $p.AllocationUnitSize = [int64]$v * 1MB }
                    'wc' { $p.WriteCacheSize = [int64]$v * 1MB }
                    default { throw "unknown space parameter $k" }
                }
            }
            New-VirtualDisk @p | Out-Null
        }
        'renamepool' {
            Set-StoragePool -FriendlyName $poolName -NewFriendlyName $a[1]
            $poolName = $a[1]
        }
        'media' { Get-MemberDisk $a[1] | Set-PhysicalDisk -MediaType $a[2] }
        'usage' { Get-MemberDisk $a[1] | Set-PhysicalDisk -Usage $a[2] }
        'optimize' { Optimize-StoragePool -FriendlyName $poolName; Wait-Jobs }
        'waitjobs' { Wait-Jobs }
        'removepool' {
            Set-StoragePool -FriendlyName $poolName -IsReadOnly $false
            Remove-StoragePool -FriendlyName $poolName -Confirm:$false
        }
        'format' {
            $d = Get-VirtualDisk -FriendlyName $a[1] | Get-Disk
            if ($d.IsOffline) { $d | Set-Disk -IsOffline $false }
            if ($d.IsReadOnly) { $d | Set-Disk -IsReadOnly $false }
            if ($d.PartitionStyle -eq 'RAW') { $d | Initialize-Disk -PartitionStyle GPT }
            $d | New-Partition -UseMaximumSize -AssignDriveLetter | Format-Volume -FileSystem NTFS -NewFileSystemLabel $a[1] -Confirm:$false | Out-Null
        }
        'file' {
            $path = Join-Path (Get-SpaceVolume $a[1]) $a[2]
            $buf = New-Object byte[] (1MB)
            $rng = [System.Random]::new(7)
            $fs = [System.IO.File]::Create($path)
            try {
                for ($k = 0; $k -lt [int]$a[3]; $k++) { $rng.NextBytes($buf); $fs.Write($buf, 0, $buf.Length) }
                $fs.Flush($true)
            } finally { $fs.Dispose() }
        }
        'delfile' { Remove-Item -Force (Join-Path (Get-SpaceVolume $a[1]) $a[2]) }
        'retrim' { Optimize-Volume -DriveLetter (Get-SpaceVolume $a[1]).TrimEnd(':') -ReTrim | Out-Null }
        'dismount' { foreach ($f in Get-Images) { if ((Get-DiskImage -ImagePath $f).Attached) { Dismount-DiskImage -ImagePath $f | Out-Null } } }
        default { throw "unknown step $step" }
    }
    $log += [ordered]@{ step = $step; started = $started.ToString('o'); seconds = [math]::Round(((Get-Date) - $started).TotalSeconds, 2) }
    "$step done"
}
$log | ConvertTo-Json -Depth 3 | Add-Content -Encoding UTF8 (Join-Path $dir 'scenario.log')
