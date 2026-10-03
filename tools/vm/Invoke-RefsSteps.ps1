<#
.SYNOPSIS
Changes a ReFS volume of New-RefsVolume.ps1 one step at a time and keeps a
copy of its VHDX after each step (write experiments, Track B4 in
docs/plan.md): each step attaches the volume, makes its change, flushes,
detaches and copies disk.vhdx to step-<i>-<op>.vhdx next to it (the
volume before the first step: step-0-base.vhdx).

-Steps lists operations separated by ';', arguments separated by '|'
(paths are relative to the volume's root):
  idle|SECONDS          attach, wait, detach (what attaching alone writes)
  touch|PATH            set the last write time to 2021-01-02 03:04:05 UTC
  write|PATH|OFF|LEN    overwrite LEN bytes at OFF with bytes 0xA5
  append|PATH|LEN       append LEN bytes 0x5A
  create|PATH|SIZE      a new file of SIZE bytes 0x3C
  mkdir|PATH
  delete|PATH
  rename|PATH|NEWNAME
  move|PATH|NEWPATH     move to another directory
  link|PATH|NEWPATH     a hard link to PATH
  attrib|PATH|ATTRS     set the attributes ([IO.FileAttributes] names)
  stream|PATH|NAME|LEN  write named stream NAME: LEN bytes 0x77
  unstream|PATH|NAME    delete named stream NAME
  integrity|PATH|on     turn integrity streams on (or off) for a file
Run on the Windows test VM only.
#>
param(
    [Parameter(Mandatory)] [string] $Name,
    [Parameter(Mandatory)] [string] $Steps,
    [string] $Root = 'C:\sstest\refs'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
if ($env:COMPUTERNAME -notin 'DESKTOP-BQ2J4NS', 'DESKTOP-ELS4LDK') { throw 'Unexpected machine' }

$dir = Join-Path $Root $Name
$vhdx = Join-Path $dir 'disk.vhdx'
if (-not (Test-Path $vhdx)) { throw "no volume $vhdx (plain volumes only)" }
Copy-Item $vhdx (Join-Path $dir 'step-0-base.vhdx') -Force
$i = 0
foreach ($step in ($Steps.Split(';') | Where-Object { $_ })) {
    $a = $step.Split('|')
    Mount-DiskImage -ImagePath $vhdx | Out-Null
    try {
        $disk = Get-DiskImage -ImagePath $vhdx | Get-Disk
        $part = Get-Partition -DiskNumber $disk.Number | Where-Object Type -eq 'Basic'
        if (-not $part.DriveLetter) { $part | Add-PartitionAccessPath -AssignDriveLetter; $part = Get-Partition -DiskNumber $disk.Number -PartitionNumber $part.PartitionNumber }
        $drive = "$($part.DriveLetter):\"
        $p = if ($a.Count -gt 1) { Join-Path $drive $a[1] } else { $null }
        switch ($a[0]) {
            'idle' { Start-Sleep ([int]$a[1]) }
            'touch' { [IO.File]::SetLastWriteTimeUtc($p, [datetime]::new(2021, 1, 2, 3, 4, 5, [DateTimeKind]::Utc)) }
            'write' {
                $fs = [IO.File]::Open($p, 'Open', 'Write')
                try { $fs.Position = [long]$a[2]; $b = New-Object byte[] ([int]$a[3]); for ($k = 0; $k -lt $b.Length; $k++) { $b[$k] = 0xA5 }; $fs.Write($b, 0, $b.Length) } finally { $fs.Close() }
            }
            'append' {
                $fs = [IO.File]::Open($p, 'Append', 'Write')
                try { $b = New-Object byte[] ([int]$a[2]); for ($k = 0; $k -lt $b.Length; $k++) { $b[$k] = 0x5A }; $fs.Write($b, 0, $b.Length) } finally { $fs.Close() }
            }
            'create' {
                [IO.Directory]::CreateDirectory((Split-Path $p)) | Out-Null
                $b = New-Object byte[] ([int]$a[2]); for ($k = 0; $k -lt $b.Length; $k++) { $b[$k] = 0x3C }
                [IO.File]::WriteAllBytes($p, $b)
            }
            'mkdir' { [IO.Directory]::CreateDirectory($p) | Out-Null }
            'delete' { Remove-Item -LiteralPath $p -Recurse -Force }
            'rename' { Rename-Item -LiteralPath $p $a[2] }
            'move' { Move-Item -LiteralPath $p (Join-Path $drive $a[2]) }
            'link' { New-Item -ItemType HardLink -Path (Join-Path $drive $a[2]) -Target $p | Out-Null }
            'attrib' { [IO.File]::SetAttributes($p, [IO.FileAttributes]$a[2]) }
            'stream' {
                $b = New-Object byte[] ([int]$a[3]); for ($k = 0; $k -lt $b.Length; $k++) { $b[$k] = 0x77 }
                Set-Content -LiteralPath $p -Stream $a[2] -Value $b -Encoding Byte
            }
            'unstream' { Remove-Item -LiteralPath $p -Stream $a[2] }
            'integrity' { Set-FileIntegrity -FileName $p -Enable ($a[2] -eq 'on') }
            default { throw "unknown step $step" }
        }
        Write-VolumeCache -DriveLetter $part.DriveLetter
        # Offline first: ReFS then checkpoints over its log (a clean volume).
        Set-Disk -Number $disk.Number -IsOffline $true
    } finally {
        Dismount-DiskImage -ImagePath $vhdx | Out-Null
    }
    $i++
    $copy = Join-Path $dir ("step-{0}-{1}.vhdx" -f $i, $a[0])
    Copy-Item $vhdx $copy -Force
    "step $i ($step): $copy"
}
