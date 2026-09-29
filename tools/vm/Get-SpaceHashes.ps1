<#
.SYNOPSIS
Attaches a round-trip pool and records what Windows reads from its main
space, as one MD5 per chunk.

C:\sstest\roundtrip\<Name> holds the pool's disk<N>.vhdx files and its
manifest.json (see Test-RoundTrip.ps1). The script attaches the disks, reads
the main space (the manifest's space) through its disk device and writes
hashes.txt there: one line "<chunk index> <md5>" per ChunkKB of the space.
The pool stays read-only unless -Writable (as attached, Windows keeps a
pool it finds read-only so), so that Windows changes nothing before the
hashes are taken. Then it detaches the disks again. Compare it with
`tools/space-hashes.py` of the same pool read on Linux to find the chunks
Windows reads differently. Run on the Windows test VM only.
#>
param(
    [Parameter(Mandatory)] [string] $Name,
    [string] $Root = 'C:\sstest\roundtrip',
    [int] $ChunkKB = 128,
    [switch] $Writable
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

if ($env:COMPUTERNAME -notin 'DESKTOP-BQ2J4NS', 'DESKTOP-ELS4LDK') { throw 'Unexpected machine' }

$dir = Join-Path $Root $Name
$manifest = Get-Content -Raw (Join-Path $dir 'manifest.json') | ConvertFrom-Json
$poolName = $manifest.pool.name
if (Get-StoragePool -FriendlyName $poolName -ErrorAction SilentlyContinue) { throw "a pool named $poolName is attached already" }
$images = @(Get-ChildItem $dir -Filter 'disk*.vhdx' | Sort-Object Name | ForEach-Object FullName)
if (-not $images) { throw "no disk images in $dir" }
try {
    foreach ($f in $images) { Mount-DiskImage -ImagePath $f | Out-Null }
    $pool = $null
    for ($i = 0; $i -lt 60 -and -not $pool; $i++) {
        Start-Sleep -Seconds 1
        $pool = Get-StoragePool -FriendlyName $poolName -ErrorAction SilentlyContinue
    }
    if (-not $pool) { throw "pool $poolName did not appear" }
    Start-Sleep -Seconds 5
    if ($Writable -and $pool.IsReadOnly) { Set-StoragePool -FriendlyName $poolName -IsReadOnly $false }
    "pool read-only: $((Get-StoragePool -FriendlyName $poolName).IsReadOnly)"
    $vd = Get-VirtualDisk -FriendlyName $manifest.space.name
    if ($vd.OperationalStatus -eq 'Detached') { $vd | Connect-VirtualDisk; Start-Sleep -Seconds 5 }
    $disk = $vd | Get-Disk
    $dev = "\\.\PhysicalDrive$($disk.Number)"
    $size = [int64]$vd.Size
    $chunk = $ChunkKB * 1KB
    $md5 = [System.Security.Cryptography.MD5]::Create()
    $buf = New-Object byte[] (4MB)
    $lines = New-Object System.Collections.Generic.List[string]
    $fs = New-Object System.IO.FileStream($dev, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite, 4096)
    try {
        for ($pos = [int64]0; $pos -lt $size; $pos += $buf.Length) {
            $n = [int][Math]::Min([int64]$buf.Length, $size - $pos)
            $got = 0
            while ($got -lt $n) {
                $r = $fs.Read($buf, $got, $n - $got)
                if ($r -le 0) { throw "short read at $($pos + $got)" }
                $got += $r
            }
            for ($c = 0; $c -lt $n; $c += $chunk) {
                $h = $md5.ComputeHash($buf, $c, [Math]::Min($chunk, $n - $c))
                $lines.Add("$(($pos + $c) / $chunk) $(-join ($h | ForEach-Object { $_.ToString('x2') }))")
            }
        }
    } finally { $fs.Dispose() }
    [System.IO.File]::WriteAllLines((Join-Path $dir 'hashes.txt'), $lines)
    "$($lines.Count) chunks of $ChunkKB KiB hashed"
} finally {
    foreach ($f in $images) { Dismount-DiskImage -ImagePath $f -ErrorAction SilentlyContinue | Out-Null }
}
