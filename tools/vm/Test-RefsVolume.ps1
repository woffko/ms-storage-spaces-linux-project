<#
.SYNOPSIS
What Windows makes of a ReFS volume image (write experiments, Track B4):
attaches it, prints its health, the modification time of small.txt when
there is one, the SHA-256 (first 16 digits) of every file outside System
Volume Information, the summary of `refsutil leak /d /v /x` (diagnose only)
and `refsutil triage /g`, and ReFS warnings and errors logged meanwhile;
flushes and detaches. Windows writes to the volume while it is attached.
Run on the Windows test VM only.
#>
param([Parameter(Mandatory)] [string] $Image)
$ErrorActionPreference = 'Stop'
if ($env:COMPUTERNAME -notin 'DESKTOP-BQ2J4NS', 'DESKTOP-ELS4LDK') { throw 'Unexpected machine' }
$t0 = Get-Date
Mount-DiskImage -ImagePath $Image | Out-Null
try {
    $d = (Get-DiskImage -ImagePath $Image | Get-Disk | Get-Partition | Where-Object Type -eq Basic).DriveLetter
    "== $Image on ${d}:"
    $v = Get-Volume -DriveLetter $d
    "health $($v.HealthStatus), $($v.OperationalStatus)"
    if (Test-Path "${d}:\small.txt") { "small.txt modified " + (Get-Item "${d}:\small.txt").LastWriteTimeUtc.ToString('o') }
    Get-ChildItem "${d}:\" -Recurse -Force -File -ErrorAction Continue | Where-Object FullName -notmatch 'System Volume' | ForEach-Object { '{0} {1}' -f $_.FullName, (Get-FileHash $_.FullName).Hash.Substring(0, 16) }
    '-- refsutil leak /d /v'
    refsutil leak "${d}:" /d /v /x 2>&1 | Select-Object -Last 15
    '-- refsutil triage /g'
    refsutil triage "${d}:" /g 2>&1 | Select-Object -Last 8
    Start-Sleep 3
    Get-WinEvent -FilterHashtable @{ LogName = 'System'; StartTime = $t0 } -ErrorAction SilentlyContinue | Where-Object { $_.ProviderName -match 'ReFS' -and $_.Level -le 3 } | ForEach-Object { 'event {0} level {1}: {2}' -f $_.Id, $_.LevelDisplayName, (($_.Message -split "`n")[0]) }
    Write-VolumeCache -DriveLetter $d
} finally {
    Dismount-DiskImage -ImagePath $Image | Out-Null
}
