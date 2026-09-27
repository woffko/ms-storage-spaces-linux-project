<#
.SYNOPSIS
Creates a Storage Spaces pool on VHDX files, removes the disks abruptly while
the verification pattern is being written (a simulated power loss for this
pool only), keeps a copy of that crashed state, then lets Windows recover the
pool and records CRC-32 checksums of every MiB of the recovered space.

-SmallWrites fills the space first and then crashes during random 4-64 KiB
writes of pattern blocks tagged "<Name>-w", which the write-back cache
absorbs (-WriteCacheMB sets its size): the crashed disks hold dirty cache
data.

Output in C:\sstest\<Name>:
  crash\disk<N>.vhdx   the disks as they were at the crash
  disk<N>.vhdx         the disks after Windows recovered the pool
  manifest.json        layout before the crash and the recovered checksums
#>
param(
    [Parameter(Mandatory)] [string] $Name,
    [int] $DiskCount = 3,
    [int] $DiskSizeMB = 8192,
    [ValidateSet('Simple', 'Mirror', 'Parity')] [string] $Resiliency = 'Mirror',
    [int] $DataCopies = 0,
    [int] $Redundancy = -1,
    [int] $Columns = 0,
    [ValidateSet('Thin', 'Fixed')] [string] $Provisioning = 'Fixed',
    [int] $SizeMB = 2048,
    [int] $CrashAfterSeconds = 20,
    [int] $WriteCacheMB = -1,
    [switch] $SmallWrites,
    [string] $Root = 'C:\sstest'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
if ($env:COMPUTERNAME -notin 'DESKTOP-BQ2J4NS', 'DESKTOP-ELS4LDK') { throw 'Unexpected machine' }

Add-Type -TypeDefinition @'
using System;
using System.IO;
public static class SsCrash {
    static ulong SplitMix(ref ulong s) {
        s += 0x9E3779B97F4A7C15UL;
        ulong z = s;
        z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9UL;
        z = (z ^ (z >> 27)) * 0x94D049BB133111EBUL;
        return z ^ (z >> 31);
    }
    public static void Fill(string device, long size, string tag) {
        byte[] t = System.Text.Encoding.ASCII.GetBytes(tag);
        byte[] magic = System.Text.Encoding.ASCII.GetBytes("SSPATTRN");
        const int chunk = 1 << 20;
        byte[] buf = new byte[chunk];
        try {
            using (var fs = new FileStream(device, FileMode.Open, FileAccess.ReadWrite, FileShare.ReadWrite, 4096, FileOptions.WriteThrough)) {
                for (long pos = 0; pos < size; pos += chunk) {
                    for (int b = 0; b < chunk; b += 4096) {
                        ulong off = (ulong)(pos + b);
                        Buffer.BlockCopy(magic, 0, buf, b, 8);
                        Buffer.BlockCopy(BitConverter.GetBytes(off), 0, buf, b + 8, 8);
                        Array.Clear(buf, b + 16, 16);
                        Buffer.BlockCopy(t, 0, buf, b + 16, Math.Min(t.Length, 16));
                        ulong s = off;
                        for (int i = 32; i < 4096; i += 8) Buffer.BlockCopy(BitConverter.GetBytes(SplitMix(ref s)), 0, buf, b + i, 8);
                    }
                    fs.Position = pos;
                    fs.Write(buf, 0, chunk);
                }
            }
        } catch (Exception) { /* the disks disappear under us */ }
    }
    public static void Block(byte[] buf, int b, ulong off, byte[] t) {
        byte[] magic = System.Text.Encoding.ASCII.GetBytes("SSPATTRN");
        Buffer.BlockCopy(magic, 0, buf, b, 8);
        Buffer.BlockCopy(BitConverter.GetBytes(off), 0, buf, b + 8, 8);
        Array.Clear(buf, b + 16, 16);
        Buffer.BlockCopy(t, 0, buf, b + 16, Math.Min(t.Length, 16));
        ulong s = off;
        for (int i = 32; i < 4096; i += 8) Buffer.BlockCopy(BitConverter.GetBytes(SplitMix(ref s)), 0, buf, b + i, 8);
    }
    // Random 4-64 KiB writes of pattern blocks until the disks disappear.
    public static void RandomWrites(string device, long size, string tag) {
        byte[] t = System.Text.Encoding.ASCII.GetBytes(tag);
        byte[] buf = new byte[64 << 10];
        var rng = new Random(4242);
        try {
            using (var fs = new FileStream(device, FileMode.Open, FileAccess.ReadWrite, FileShare.ReadWrite, 4096, FileOptions.WriteThrough)) {
                while (true) {
                    int blocks = rng.Next(1, 17);
                    long pos = (long)(rng.NextDouble() * (size / 4096 - blocks)) * 4096;
                    for (int b = 0; b < blocks; b++) Block(buf, b * 4096, (ulong)(pos + b * 4096), t);
                    fs.Position = pos;
                    fs.Write(buf, 0, blocks * 4096);
                }
            }
        } catch (Exception) { /* the disks disappear under us */ }
    }
    static uint[] table = MakeTable();
    static uint[] MakeTable() {
        var t = new uint[256];
        for (uint i = 0; i < 256; i++) { uint c = i; for (int k = 0; k < 8; k++) c = (c & 1) != 0 ? 0xEDB88320u ^ (c >> 1) : c >> 1; t[i] = c; }
        return t;
    }
    public static string MibCrcs(string device, long size) {
        var sb = new System.Text.StringBuilder();
        byte[] buf = new byte[1 << 20];
        using (var fs = new FileStream(device, FileMode.Open, FileAccess.Read, FileShare.ReadWrite)) {
            for (long pos = 0; pos < size; pos += buf.Length) {
                fs.Position = pos;
                int got = 0;
                while (got < buf.Length) { int r = fs.Read(buf, got, buf.Length - got); if (r <= 0) break; got += r; }
                uint c = 0xFFFFFFFFu;
                for (int i = 0; i < got; i++) c = table[(c ^ buf[i]) & 0xFF] ^ (c >> 8);
                sb.Append((~c).ToString("x8"));
                sb.Append(' ');
            }
        }
        return sb.ToString().Trim();
    }
}
'@

$dir = Join-Path $Root $Name
if (Test-Path $dir) { throw "Test pool directory already exists: $dir" }
New-Item -ItemType Directory -Path (Join-Path $dir 'crash') | Out-Null
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
$vdParams = @{ StoragePoolFriendlyName = $poolName; FriendlyName = $Name; ResiliencySettingName = $Resiliency
    ProvisioningType = $Provisioning; Size = [int64]$SizeMB * 1MB }
if ($DataCopies -gt 0) { $vdParams.NumberOfDataCopies = $DataCopies }
if ($Redundancy -ge 0) { $vdParams.PhysicalDiskRedundancy = $Redundancy }
if ($Columns -gt 0) { $vdParams.NumberOfColumns = $Columns }
if ($WriteCacheMB -ge 0) { $vdParams.WriteCacheSize = [int64]$WriteCacheMB * 1MB }
$vd = New-VirtualDisk @vdParams
$disk = $vd | Get-Disk
if ($disk.IsOffline) { $disk | Set-Disk -IsOffline $false }
if ($disk.IsReadOnly) { $disk | Set-Disk -IsReadOnly $false }
$vd = Get-VirtualDisk -FriendlyName $Name
$vdGuid = if ($vd.ObjectId -match '\{([0-9a-fA-F-]+)\}"?$') { $Matches[1].ToLowerInvariant() } else { $null }
$size = $vd.Size

# Write in a separate runspace and pull the disks while it runs.
$ps = [powershell]::Create()
if ($SmallWrites) {
    [SsCrash]::Fill("\\.\PhysicalDrive$($disk.Number)", $size, $Name)
    [void]$ps.AddScript({ param($d, $s, $t) [SsCrash]::RandomWrites($d, $s, $t) }).AddArgument("\\.\PhysicalDrive$($disk.Number)").AddArgument($size).AddArgument("$Name-w")
} else {
    [void]$ps.AddScript({ param($d, $s, $t) [SsCrash]::Fill($d, $s, $t) }).AddArgument("\\.\PhysicalDrive$($disk.Number)").AddArgument($size).AddArgument($Name)
}
$handle = $ps.BeginInvoke()
Start-Sleep -Seconds $CrashAfterSeconds
foreach ($f in $images) { Dismount-DiskImage -ImagePath $f | Out-Null }
try { [void]$ps.EndInvoke($handle) } catch { }
$ps.Dispose()

for ($i = 0; $i -lt $DiskCount; $i++) {
    Copy-Item $images[$i] (Join-Path $dir ("crash\disk{0}.vhdx" -f $i))
}

# Let Windows recover the pool, then checksum the recovered space.
foreach ($f in $images) { Mount-DiskImage -ImagePath $f -NoDriveLetter | Out-Null }
Start-Sleep -Seconds 5
$pool = Get-StoragePool -FriendlyName $poolName
if ($pool.IsReadOnly) { $pool | Set-StoragePool -IsReadOnly $false }
$vd = Get-VirtualDisk -FriendlyName $Name
if ($vd.OperationalStatus -eq 'Detached') { $vd | Connect-VirtualDisk }
Start-Sleep -Seconds 10
$vd = Get-VirtualDisk -FriendlyName $Name
$disk = $vd | Get-Disk
$crcs = [SsCrash]::MibCrcs("\\.\PhysicalDrive$($disk.Number)", $size)
$poolDisks = foreach ($f in $images) {
    $pd = Get-PhysicalDisk | Where-Object DeviceId -eq "$((Get-DiskImage -ImagePath $f).Number)"
    $guid = if ($pd.ObjectId -match 'PD:\{([0-9a-fA-F-]+)\}') { $Matches[1].ToLowerInvariant() } else { $null }
    [ordered]@{ image = Split-Path $f -Leaf; unique_id = $pd.UniqueId; spaces_guid = $guid; size = $pd.Size }
}
$manifest = [ordered]@{
    name = $Name
    kind = 'crash'
    windows_build = [Environment]::OSVersion.Version.ToString()
    crash_after_seconds = $CrashAfterSeconds
    small_writes = [bool]$SmallWrites
    write_cache = $vd.WriteCacheSize
    space = [ordered]@{ name = $Name; guid = $vdGuid; size = $size; resiliency = $vd.ResiliencySettingName
        status_after_recovery = "$($vd.OperationalStatus)"; health_after_recovery = "$($vd.HealthStatus)" }
    disks = @($poolDisks)
    recovered_mib_crc32 = $crcs
}
$manifest | ConvertTo-Json -Depth 5 | Set-Content -Encoding UTF8 (Join-Path $dir 'manifest.json')
foreach ($f in $images) { Dismount-DiskImage -ImagePath $f | Out-Null }
"OK $dir"
