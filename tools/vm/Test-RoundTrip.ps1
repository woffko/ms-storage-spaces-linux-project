<#
.SYNOPSIS
Attaches a pool whose disks were changed outside Windows and records what
Windows makes of it.

C:\sstest\roundtrip\<Name> holds the pool's disk<N>.vhdx files (made with
tools/raw2vhdx.py) and its manifest.json from tools/vm/New-TestPool.ps1.
The script attaches the disks and writes roundtrip.json there:
  attached   pool, space and disk health as Windows first sees them
  connected  after clearing a read-only pool and connecting detached spaces
  repaired   after Repair-VirtualDisk of every space (not with -NoRepair)
  extents    Get-PhysicalExtent of every space
  checks     the verification pattern of every pattern space, chkdsk and
             the file hashes of an NTFS space
With -Written "OFFKB:LENKB:TAG;..." the main space must hold the pattern
with TAG in those ranges (written from Linux) and its own tag elsewhere.
With -ProbeStrideKB N it also reads the 4 KiB block at every N KiB of the
main space three times (4 MiB sequential reads, 4 KiB reads, 4 MiB again)
and counts the tags found there ("probe"): after mirror copies were made
to differ, this shows which copy Windows reads.
With -WaitSeconds it keeps the pool attached that long before the checks
(state "waited"), so that background work of Windows can run. Then it
detaches the disks again. Run on the Windows test VM only.
#>
param(
    [Parameter(Mandatory)] [string] $Name,
    [string] $Root = 'C:\sstest\roundtrip',
    [switch] $NoRepair,
    [int] $WaitSeconds = 0,
    [int] $ProbeStrideKB = 0,
    [string] $Written = ''
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

if ($env:COMPUTERNAME -notin 'DESKTOP-BQ2J4NS', 'DESKTOP-ELS4LDK') { throw 'Unexpected machine' }

Add-Type -TypeDefinition @'
using System;
using System.IO;
public static class SsVerify {
    static ulong SplitMix(ref ulong s) {
        s += 0x9E3779B97F4A7C15UL;
        ulong z = s;
        z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9UL;
        z = (z ^ (z >> 27)) * 0x94D049BB133111EBUL;
        return z ^ (z >> 31);
    }
    static void FillBlock(byte[] buf, ulong offset, byte[] tag) {
        Buffer.BlockCopy(System.Text.Encoding.ASCII.GetBytes("SSPATTRN"), 0, buf, 0, 8);
        Buffer.BlockCopy(BitConverter.GetBytes(offset), 0, buf, 8, 8);
        Array.Clear(buf, 16, 16);
        Buffer.BlockCopy(tag, 0, buf, 16, Math.Min(tag.Length, 16));
        ulong s = offset;
        for (int i = 32; i < 4096; i += 8) Buffer.BlockCopy(BitConverter.GetBytes(SplitMix(ref s)), 0, buf, i, 8);
    }
    // Offset of the first 4 KiB block in [0, size) that does not hold the
    // pattern, or -1.
    public static long Verify(string device, long size, string tag) {
        return VerifyRanges(device, size, tag, new long[0], new long[0], new string[0]);
    }
    // As Verify, with blocks in [starts[i], ends[i]) tagged tags[i].
    public static long VerifyRanges(string device, long size, string tag, long[] starts, long[] ends, string[] tags) {
        byte[] t = System.Text.Encoding.ASCII.GetBytes(tag);
        const int chunk = 4 << 20;
        byte[] buf = new byte[chunk];
        byte[] expected = new byte[4096];
        using (var fs = new FileStream(device, FileMode.Open, FileAccess.Read, FileShare.ReadWrite, 4096)) {
            for (long pos = 0; pos < size; pos += chunk) {
                int n = (int)Math.Min(chunk, size - pos);
                fs.Position = pos;
                for (int got = 0; got < n; ) {
                    int r = fs.Read(buf, got, n - got);
                    if (r <= 0) return pos + got;
                    got += r;
                }
                for (int b = 0; b < n; b += 4096) {
                    byte[] bt = t;
                    for (int r = 0; r < starts.Length; r++)
                        if (pos + b >= starts[r] && pos + b < ends[r]) bt = System.Text.Encoding.ASCII.GetBytes(tags[r]);
                    FillBlock(expected, (ulong)(pos + b), bt);
                    for (int i = 0; i < 4096; i++) if (buf[b + i] != expected[i]) return pos + b;
                }
            }
        }
        return -1;
    }
    // Tags (bytes 16..32) of the 4 KiB blocks at every stride bytes in
    // [0, size), read in chunk-sized reads.
    public static string[] Tags(string device, long size, long stride, int chunk) {
        var tags = new System.Collections.Generic.List<string>();
        byte[] buf = new byte[Math.Max(chunk, 4096)];
        using (var fs = new FileStream(device, FileMode.Open, FileAccess.Read, FileShare.ReadWrite, 4096)) {
            if (chunk <= 4096) {
                for (long at = 0; at < size; at += stride) {
                    fs.Position = at;
                    for (int got = 0; got < 4096; ) got += fs.Read(buf, got, 4096 - got);
                    tags.Add(System.Text.Encoding.ASCII.GetString(buf, 16, 16).TrimEnd('\0'));
                }
            } else {
                for (long pos = 0; pos < size; pos += chunk) {
                    int n = (int)Math.Min(chunk, size - pos);
                    fs.Position = pos;
                    for (int got = 0; got < n; ) got += fs.Read(buf, got, n - got);
                    for (long at = (pos + stride - 1) / stride * stride; at < pos + n; at += stride)
                        tags.Add(System.Text.Encoding.ASCII.GetString(buf, (int)(at - pos) + 16, 16).TrimEnd('\0'));
                }
            }
        }
        return tags.ToArray();
    }
}
'@

$dir = Join-Path $Root $Name
$manifest = Get-Content (Join-Path $dir 'manifest.json') -Raw | ConvertFrom-Json
$poolName = $manifest.pool.name
if (Get-StoragePool -FriendlyName $poolName -ErrorAction SilentlyContinue) { throw "a pool named $poolName is attached already" }
$images = @(Get-ChildItem $dir -Filter 'disk*.vhdx' | Sort-Object Name | ForEach-Object FullName)
if (-not $images) { throw "no disk images in $dir" }

function Get-State {
    $p = Get-StoragePool -FriendlyName $poolName
    [ordered]@{
        pool   = [ordered]@{
            health = "$($p.HealthStatus)"; operational = "$($p.OperationalStatus)"
            read_only = $p.IsReadOnly; read_only_reason = "$($p.ReadOnlyReason)"
        }
        spaces = @($p | Get-VirtualDisk | ForEach-Object {
            [ordered]@{
                name = $_.FriendlyName; health = "$($_.HealthStatus)"; operational = "$($_.OperationalStatus)"
                detached_reason = "$($_.DetachedReason)"; manual_attach = $_.IsManualAttach
            }
        })
        disks  = @($p | Get-PhysicalDisk | ForEach-Object {
            [ordered]@{
                unique_id = $_.UniqueId; health = "$($_.HealthStatus)"
                operational = "$($_.OperationalStatus)"; usage = "$($_.Usage)"
            }
        })
        jobs   = @(Get-StorageJob | ForEach-Object {
            [ordered]@{
                name = $_.Name; state = "$($_.JobState)"; percent = $_.PercentComplete
                bytes = $_.BytesProcessed; total = $_.BytesTotal
            }
        })
    }
}

$result = [ordered]@{ name = $Name; windows_build = [Environment]::OSVersion.Version.ToString() }
try {
    foreach ($f in $images) { Mount-DiskImage -ImagePath $f | Out-Null }
    $pool = $null
    for ($i = 0; $i -lt 60 -and -not $pool; $i++) {
        Start-Sleep -Seconds 1
        $pool = Get-StoragePool -FriendlyName $poolName -ErrorAction SilentlyContinue
    }
    if (-not $pool) { throw "pool $poolName did not appear" }
    Start-Sleep -Seconds 5
    $result.attached = Get-State

    if ($pool.IsReadOnly) { Set-StoragePool -FriendlyName $poolName -IsReadOnly $false }
    Get-StoragePool -FriendlyName $poolName | Get-VirtualDisk |
        Where-Object OperationalStatus -eq 'Detached' | Connect-VirtualDisk
    Start-Sleep -Seconds 5
    $result.connected = Get-State

    if ($WaitSeconds -gt 0) {
        Start-Sleep -Seconds $WaitSeconds
        $result.waited = Get-State
    }

    if (-not $NoRepair) {
        foreach ($vd in Get-StoragePool -FriendlyName $poolName | Get-VirtualDisk) {
            Repair-VirtualDisk -FriendlyName $vd.FriendlyName
        }
        $result.repaired = Get-State
    }

    $result.extents = [ordered]@{}
    foreach ($vd in Get-StoragePool -FriendlyName $poolName | Get-VirtualDisk) {
        $result.extents[$vd.FriendlyName] = @($vd | Get-PhysicalExtent | ForEach-Object {
            [ordered]@{
                column = $_.ColumnNumber; copy = $_.CopyNumber; size = $_.Size
                virtual_offset = $_.VirtualDiskOffset; physical_offset = $_.PhysicalDiskOffset
                disk_unique_id = $_.PhysicalDiskUniqueId; status = "$($_.OperationalStatus)"
            }
        })
    }

    $checks = @()
    $patterned = @()
    if ($manifest.pattern) { $patterned += , @($manifest.space.name, [int64]$manifest.pattern_size) }
    foreach ($e in @($manifest.extra_spaces)) { if ($e) { $patterned += , @($e.name, [int64]$e.size) } }
    $starts = @(); $ends = @(); $tags = @()
    foreach ($w in ($Written.Split(';') | Where-Object { $_ })) {
        $f = $w.Split(':')
        $starts += [int64]$f[0] * 1KB; $ends += ([int64]$f[0] + [int64]$f[1]) * 1KB; $tags += $f[2]
    }
    foreach ($p in $patterned) {
        $disk = Get-VirtualDisk -FriendlyName $p[0] | Get-Disk
        if ($disk.IsOffline) { $disk | Set-Disk -IsOffline $false }
        $dev = "\\.\PhysicalDrive$($disk.Number)"
        $bad = if ($p[0] -eq $manifest.space.name -and $starts) {
            [SsVerify]::VerifyRanges($dev, $p[1], $p[0], [int64[]]$starts, [int64[]]$ends, [string[]]$tags)
        } else { [SsVerify]::Verify($dev, $p[1], $p[0]) }
        $checks += [ordered]@{ space = $p[0]; kind = 'pattern'; bytes = $p[1]; written = $Written; first_mismatch = $bad; ok = ($bad -lt 0) }
    }
    if ($manifest.files) {
        $disk = Get-VirtualDisk -FriendlyName $manifest.space.name | Get-Disk
        if ($disk.IsOffline) { $disk | Set-Disk -IsOffline $false }
        $part = $disk | Get-Partition | Where-Object Type -eq 'Basic' | Select-Object -First 1
        $letter = $part.DriveLetter
        if (-not $letter) { $part | Add-PartitionAccessPath -AssignDriveLetter; $letter = (Get-Partition -DiskNumber $disk.Number -PartitionNumber $part.PartitionNumber).DriveLetter }
        $chkdsk = & chkdsk.exe "$($letter):" 2>&1 | Out-String
        $chkdskExit = $LASTEXITCODE
        $mismatch = @()
        foreach ($f in $manifest.files) {
            $path = "$($letter):\" + $f.path.Replace('/', '\')
            $hash = if (Test-Path -LiteralPath $path) { (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant() } else { 'missing' }
            if ($hash -ne $f.sha256) { $mismatch += $f.path }
        }
        $checks += [ordered]@{
            space = $manifest.space.name; kind = 'ntfs'; chkdsk_exit = $chkdskExit; chkdsk = $chkdsk
            files = @($manifest.files).Count; mismatching = $mismatch; ok = ($chkdskExit -eq 0 -and -not $mismatch)
        }
        Remove-PartitionAccessPath -DiskNumber $disk.Number -PartitionNumber $part.PartitionNumber -AccessPath "$($letter):\"
    }
    $result.checks = $checks
    if ($ProbeStrideKB -gt 0) {
        $disk = Get-VirtualDisk -FriendlyName $manifest.space.name | Get-Disk
        $dev = "\\.\PhysicalDrive$($disk.Number)"
        $size = [int64]$manifest.pattern_size
        $probe = @()
        foreach ($pass in @(@('sequential', 4MB), @('4k', 4096), @('sequential-again', 4MB))) {
            $tags = [SsVerify]::Tags($dev, $size, [int64]$ProbeStrideKB * 1KB, $pass[1])
            $counts = [ordered]@{}
            foreach ($g in $tags | Group-Object | Sort-Object Name) { $counts[$g.Name] = $g.Count }
            $probe += [ordered]@{ pass = $pass[0]; counts = $counts; tags = ($tags -join ',') }
        }
        $result.probe = $probe
    }
} catch {
    $result.error = "$_"
} finally {
    foreach ($f in $images) { Dismount-DiskImage -ImagePath $f -ErrorAction SilentlyContinue | Out-Null }
    $result | ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 (Join-Path $dir 'roundtrip.json')
}
if ($result.error) { throw $result.error }
$failed = @($result.checks | Where-Object { -not $_.ok })
"roundtrip $Name`: $(@($result.checks).Count) checks, $($failed.Count) failed"
