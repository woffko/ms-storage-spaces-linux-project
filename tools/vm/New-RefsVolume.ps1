<#
.SYNOPSIS
Creates a ReFS volume on a VHDX file with a known file tree and records
Windows' view of every file (Track B corpus, docs/plan.md).

C:\sstest\refs\<Name>\disk.vhdx (a dynamic VHDX of 52 GiB: Dev Drive needs
50 GB; only what ReFS writes takes space) gets a GPT, one partition and a
Dev Drive (ReFS; Windows 11 Pro formats ReFS only as a Dev Drive). Then
the tree of the scenario is written and manifest.json lists every file and
directory: path, kind, size, SHA-256 of the data and of each alternate
stream, attributes, the four timestamps, hard link groups, link targets,
sparse ranges. The VHDX is detached at the end. Run on the Windows test VM
only.

Scenarios:
  basic   names (Unicode, 240 characters, spaces), sizes around cluster and page
          boundaries, larger and fragmented files, a directory of 5000
          files, a sparse file, alternate streams, hard links, symbolic
          links and a junction, attributes, set timestamps, block-cloned
          copies, deleted and renamed files
  features  compressible text, random and zero files, a file with stream
          snapshots taken between overwrites, two separately written
          identical files deduplicated by refsutil, and then the volume
          compressed by refsutil (-Compression LZ4 or ZSTD, with
          -CompressionLevel and -ChunkSize; NONE leaves it uncompressed)
  empty   the freshly formatted volume only
#>
param(
    [Parameter(Mandatory)] [string] $Name,
    [ValidateSet('basic', 'features', 'empty')] [string] $Scenario = 'basic',
    [ValidateSet('NONE', 'LZ4', 'ZSTD')] [string] $Compression = 'NONE',
    [int] $CompressionLevel = 0,
    [long] $ChunkSize = 0,
    [ValidateSet(4096, 65536)] [int] $ClusterSize = 4096,
    [switch] $Sha256Checksums,
    [switch] $IntegrityStreams,
    [string] $Root = 'C:\sstest\refs'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
if ($env:COMPUTERNAME -notin 'DESKTOP-BQ2J4NS', 'DESKTOP-ELS4LDK') { throw 'Unexpected machine' }

$dir = Join-Path $Root $Name
if (Test-Path $dir) { throw "exists: $dir" }
New-Item -ItemType Directory -Force $dir | Out-Null
$vhdx = Join-Path $dir 'disk.vhdx'
"create vdisk file=`"$vhdx`" maximum=53248 type=expandable" | Set-Content -Encoding ASCII (Join-Path $dir 'diskpart.txt')
diskpart /s (Join-Path $dir 'diskpart.txt') | Out-Null
Mount-DiskImage -ImagePath $vhdx | Out-Null
try {
    $disk = Get-DiskImage -ImagePath $vhdx | Get-Disk
    Initialize-Disk -Number $disk.Number -PartitionStyle GPT
    $part = New-Partition -DiskNumber $disk.Number -UseMaximumSize -AssignDriveLetter
    $format = @{ Partition = $part; DevDrive = $true; NewFileSystemLabel = $Name; AllocationUnitSize = $ClusterSize; Force = $true; Confirm = $false }
    if ($Sha256Checksums) { $format.SHA256Checksums = $true }
    if ($IntegrityStreams) { $format.SetIntegrityStreams = $true }
    Format-Volume @format | Out-Null
    $part = Get-Partition -DiskNumber $disk.Number -PartitionNumber $part.PartitionNumber
    $drive = "$($part.DriveLetter):\"

    # Deterministic content: SplitMix64 bytes seeded from the file's name.
    Add-Type -TypeDefinition @'
using System;
using System.IO;
public static class RefsGen {
    static ulong Next(ref ulong s) {
        s += 0x9E3779B97F4A7C15UL;
        ulong z = s;
        z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9UL;
        z = (z ^ (z >> 27)) * 0x94D049BB133111EBUL;
        return z ^ (z >> 31);
    }
    public static ulong Seed(string name) {
        ulong h = 1469598103934665603UL;
        foreach (char c in name) { h ^= c; h *= 1099511628211UL; }
        return h;
    }
    // Appends `length` bytes of the stream seeded with `seed`, from byte `from`.
    public static void Write(string path, ulong seed, long from, long length, bool append) {
        using (var fs = new FileStream(path, append ? FileMode.Append : FileMode.Create, FileAccess.Write)) {
            Fill(fs, seed, from, length);
        }
    }
    // Text of words picked by the stream seeded with `seed` (compresses
    // about three to one).
    public static void WriteText(string path, ulong seed, long length) {
        string[] words = { "the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "storage", "spaces",
            "resilient", "file", "system", "cluster", "extent", "stream", "metadata", "checksum", "page", "tree" };
        var text = new System.Text.StringBuilder();
        ulong s = seed;
        long line = 0;
        while (text.Length < length) {
            text.Append(line++).Append(':');
            for (int w = 0; w < 10; w++) text.Append(' ').Append(words[(int)(Next(ref s) % (ulong)words.Length)]);
            text.Append('\n');
        }
        byte[] bytes = System.Text.Encoding.ASCII.GetBytes(text.ToString(0, (int)length));
        File.WriteAllBytes(path, bytes);
    }
    public static void Fill(Stream fs, ulong seed, long from, long length) {
        byte[] buf = new byte[1 << 20];
        long pos = 0;
        ulong s = seed + (ulong)(from / 8);
        int skip = (int)(from % 8);
        while (pos < length) {
            int n = (int)Math.Min(buf.Length, length - pos);
            for (int i = 0; i < n; i += 8) {
                byte[] w = BitConverter.GetBytes(Next(ref s));
                for (int k = 0; k < 8 && i + k < n; k++) buf[i + k] = w[k];
            }
            if (skip > 0) { Buffer.BlockCopy(buf, skip, buf, 0, n - skip); skip = 0; }
            fs.Write(buf, 0, n);
            pos += n;
        }
    }
}
'@
    function New-File([string] $rel, [long] $size) {
        $p = Join-Path $drive $rel
        New-Item -ItemType Directory -Force (Split-Path $p) | Out-Null
        [RefsGen]::Write($p, [RefsGen]::Seed($rel), 0, $size, $false)
    }

    function New-Text([string] $rel, [long] $size, [string] $seed = $rel) {
        $p = Join-Path $drive $rel
        [IO.Directory]::CreateDirectory((Split-Path $p)) | Out-Null
        [RefsGen]::WriteText($p, [RefsGen]::Seed($seed), $size)
    }
    function Invoke-Native([string] $what) {
        $out = & cmd /c "$what 2>&1"
        if ($LASTEXITCODE) { throw "$what failed: $out" }
        $out
    }

    if ($Scenario -eq 'features') {
        foreach ($s in 1000, 65536, 100000, 1048576, 10485760) { New-Text "compress\text_$s.txt" $s }
        New-File 'compress\random.bin' 3145728
        [IO.File]::WriteAllBytes((Join-Path $drive 'compress\zeros.bin'), (New-Object byte[] 5MB))
        # Stream snapshots between overwrites of the live data.
        New-Text 'snap\file.txt' 1048576
        $f = Join-Path $drive 'snap\file.txt'
        Invoke-Native "refsutil streamsnapshot /c s1 `"$f`"" | Out-Null
        [RefsGen]::Write($f, [RefsGen]::Seed('snap overwrite 1'), 0, 65536, $true)
        $fs = [IO.File]::Open($f, 'Open', 'Write')
        try { $fs.Position = 131072; [RefsGen]::Fill($fs, [RefsGen]::Seed('snap overwrite 2'), 0, 65536) } finally { $fs.Close() }
        Invoke-Native "refsutil streamsnapshot /c s2 `"$f`"" | Out-Null
        $fs = [IO.File]::Open($f, 'Open', 'Write')
        try { $fs.Position = 524288; [RefsGen]::Fill($fs, [RefsGen]::Seed('snap overwrite 3'), 0, 4096) } finally { $fs.Close() }
        # Two identical files written separately, then deduplicated.
        New-Text 'dedup\a.txt' 4194304 'dedup'
        New-Text 'dedup\b.txt' 4194304 'dedup'
        New-File 'dedup\c.bin' 2097152
        [RefsGen]::Write((Join-Path $drive 'dedup\d.bin'), [RefsGen]::Seed('dedup\c.bin'), 0, 2097152, $false)
        Write-VolumeCache -DriveLetter $part.DriveLetter
        Invoke-Native "refsutil dedup $($part.DriveLetter): /d" | Out-Null
        if ($Compression -ne 'NONE') {
            $c = "refsutil compression /c /f $Compression"
            if ($CompressionLevel) { $c += " /e $CompressionLevel" }
            if ($ChunkSize) { $c += " /cs $ChunkSize" }
            Invoke-Native "$c $($part.DriveLetter):" | Out-Null
        }
        New-Text 'last.txt' 100
    }

    if ($Scenario -eq 'basic') {
        # Names.
        foreach ($n in 'plain.txt', 'with spaces.txt', 'кириллица.txt', '中文文件.txt', 'emoji 😀.txt', ('l' * 236 + '.txt')) {
            New-File "names\$n" 1000
        }
        New-File 'deep\a\b\c\d\e\f\g\h\leaf.bin' 12345
        # Sizes around clusters, pages and resident data.
        foreach ($s in 0, 1, 100, 1000, 4095, 4096, 4097, 16383, 16384, 16385, 65535, 65536, 65537, 1048576, 10485760) {
            New-File "sizes\size_$s.bin" $s
        }
        New-File 'big\big64m.bin' 67108864
        # Fragmented: two files grown in turns.
        $fa = Join-Path $drive 'frag\a.bin'; $fb = Join-Path $drive 'frag\b.bin'
        New-Item -ItemType Directory -Force (Join-Path $drive 'frag') | Out-Null
        for ($i = 0; $i -lt 64; $i++) {
            [RefsGen]::Write($fa, [RefsGen]::Seed('frag\a.bin'), $i * 65536, 65536, $i -gt 0)
            [RefsGen]::Write($fb, [RefsGen]::Seed('frag\b.bin'), $i * 65536, 65536, $i -gt 0)
        }
        # A large directory.
        $many = Join-Path $drive 'many'
        New-Item -ItemType Directory -Force $many | Out-Null
        for ($i = 0; $i -lt 5000; $i++) { [RefsGen]::Write((Join-Path $many ('f{0:d5}.txt' -f $i)), [RefsGen]::Seed("many\$i"), 0, 64 + ($i % 200), $false) }
        # Sparse: 1 GiB with data at 0, 300 MiB and the end.
        $sp = Join-Path $drive 'sparse\sparse.bin'
        New-Item -ItemType Directory -Force (Split-Path $sp) | Out-Null
        New-Item -ItemType File $sp | Out-Null
        fsutil sparse setflag $sp | Out-Null
        $fs = [IO.File]::Open($sp, 'Open', 'Write')
        try {
            $fs.SetLength(1GB)
            foreach ($at in 0, 300MB, (1GB - 1MB)) { $fs.Position = $at; [RefsGen]::Fill($fs, [RefsGen]::Seed("sparse@$at"), 0, 1MB) }
        } finally { $fs.Close() }
        fsutil sparse setrange $sp ([long]1MB) ([long](299MB)) | Out-Null
        # Alternate data streams.
        New-File 'streams\host.txt' 2000
        $h = Join-Path $drive 'streams\host.txt'
        Set-Content -Path $h -Stream small -Value 'a small stream' -NoNewline
        $big = New-Object byte[] 200000; (New-Object Random 7).NextBytes($big); Set-Content -Path $h -Stream big -Value $big -Encoding Byte
        # Links.
        New-File 'links\target.txt' 3000
        New-Item -ItemType HardLink -Path (Join-Path $drive 'links\hard1.txt') -Target (Join-Path $drive 'links\target.txt') | Out-Null
        New-Item -ItemType HardLink -Path (Join-Path $drive 'names\hard2.txt') -Target (Join-Path $drive 'links\target.txt') | Out-Null
        New-Item -ItemType SymbolicLink -Path (Join-Path $drive 'links\sym_file') -Target (Join-Path $drive 'links\target.txt') | Out-Null
        cmd /c mklink "$(Join-Path $drive 'links\sym_rel')" target.txt | Out-Null
        New-Item -ItemType SymbolicLink -Path (Join-Path $drive 'links\sym_dir') -Target (Join-Path $drive 'deep') | Out-Null
        New-Item -ItemType Junction -Path (Join-Path $drive 'links\junction') -Target (Join-Path $drive 'sizes') | Out-Null
        # Attributes and timestamps.
        foreach ($a in 'ReadOnly', 'Hidden', 'System') {
            New-File "attrs\$($a.ToLower()).txt" 10
            [IO.File]::SetAttributes((Join-Path $drive "attrs\$($a.ToLower()).txt"), [IO.FileAttributes]$a)
        }
        New-File 'attrs\times.txt' 10
        $t = Get-Item -Force (Join-Path $drive 'attrs\times.txt')
        $t.CreationTimeUtc = [datetime]::new(2001, 2, 3, 4, 5, 6, [DateTimeKind]::Utc)
        $t.LastWriteTimeUtc = [datetime]::new(2010, 11, 12, 13, 14, 15, [DateTimeKind]::Utc)
        $t.LastAccessTimeUtc = [datetime]::new(2020, 1, 1, 0, 0, 0, [DateTimeKind]::Utc)
        # Block cloning: copies of the 10 MiB file (Copy-Item clones on a Dev Drive).
        New-Item -ItemType Directory -Force (Join-Path $drive 'clones') | Out-Null
        Copy-Item (Join-Path $drive 'sizes\size_10485760.bin') (Join-Path $drive 'clones\copy1.bin')
        Copy-Item (Join-Path $drive 'sizes\size_10485760.bin') (Join-Path $drive 'clones\copy2.bin')
        # Deleted and renamed.
        New-File 'gone\deleted.txt' 5000; Remove-Item (Join-Path $drive 'gone\deleted.txt')
        New-File 'gone\old_name.txt' 5000; Rename-Item (Join-Path $drive 'gone\old_name.txt') 'new_name.txt'
    }

    # Windows' view of what is on disk: ReFS updates a directory's entry in
    # its parent lazily, so the listing is taken after detaching and
    # attaching the volume again. Detaching the image does not wait for
    # ReFS's cache (the last writes went missing): flush it first.
    Write-VolumeCache -DriveLetter $part.DriveLetter
    Dismount-DiskImage -ImagePath $vhdx | Out-Null
    Mount-DiskImage -ImagePath $vhdx | Out-Null
    $disk = Get-DiskImage -ImagePath $vhdx | Get-Disk
    $part = Get-Partition -DiskNumber $disk.Number | Where-Object Type -eq 'Basic'
    if (-not $part.DriveLetter) { $part | Add-PartitionAccessPath -AssignDriveLetter; $part = Get-Partition -DiskNumber $disk.Number -PartitionNumber $part.PartitionNumber }
    $drive = "$($part.DriveLetter):\"
    $last = @{ basic = 'gone\new_name.txt'; features = 'last.txt' }[$Scenario]
    if ($last -and -not (Test-Path -LiteralPath (Join-Path $drive $last))) { throw 'the last writes are missing after reattaching' }
    # (Get-FileHash of Windows PowerShell 5.1 cannot open "file:stream".)
    function Get-StreamHash([string] $path, [string] $stream) {
        $bytes = [byte[]](Get-Content -LiteralPath $path -Stream $stream -Encoding Byte -Raw -Force)
        if ($null -eq $bytes) { $bytes = [byte[]]@() }
        ([BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash($bytes)) -replace '-', '').ToLowerInvariant()
    }
    # Windows' view of everything (links are listed, not followed).
    $entries = New-Object System.Collections.Generic.List[object]
    $ids = @{}
    $walk = New-Object System.Collections.Generic.List[object]
    $queue = New-Object System.Collections.Generic.Queue[string]
    $queue.Enqueue($drive)
    while ($queue.Count) {
        foreach ($item in Get-ChildItem -LiteralPath $queue.Dequeue() -Force) {
            if ($item.Name -eq 'System Volume Information') { continue }
            $walk.Add($item)
            if ($item.PSIsContainer -and -not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { $queue.Enqueue($item.FullName) }
        }
    }
    $walk | ForEach-Object {
            $item = $_
            $rel = $item.FullName.Substring($drive.Length)
            $e = [ordered]@{
                path = $rel.Replace('\', '/'); kind = if ($item.PSIsContainer) { 'dir' } else { 'file' }
                attributes = [int]$item.Attributes
                created = $item.CreationTimeUtc.ToFileTimeUtc(); written = $item.LastWriteTimeUtc.ToFileTimeUtc()
                accessed = $item.LastAccessTimeUtc.ToFileTimeUtc()
            }
            $link = $item.LinkType -in 'SymbolicLink', 'Junction'
            if ($link) { $e.link_type = "$($item.LinkType)"; $e.link_target = "$($item.Target)" }
            if (-not $item.PSIsContainer -and -not $link) {
                $e.size = $item.Length
                $e.sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $item.FullName).Hash.ToLowerInvariant()
                $streams = @(Get-Item -LiteralPath $item.FullName -Stream * -Force | Where-Object Stream -ne ':$DATA')
                if ($streams) {
                    $e.streams = @($streams | ForEach-Object {
                        # Stream snapshots are listed as "<name>:$SNAPSHOT" and
                        # read by their name.
                        [ordered]@{ name = $_.Stream; size = $_.Length
                            sha256 = Get-StreamHash $item.FullName ($_.Stream -replace ':\$SNAPSHOT$', '') } })
                }
                $id = (fsutil file queryfileid $item.FullName) -replace '.*: ', ''
                if ($ids.ContainsKey($id)) { $e.hard_link_of = $ids[$id] } else { $ids[$id] = $e.path }
                if ($item.Attributes -band [IO.FileAttributes]::SparseFile) {
                    $e.sparse_ranges = @(fsutil sparse queryrange $item.FullName | Where-Object { $_ -match 'Offset' } |
                        ForEach-Object { if ($_ -match 'Offset: 0x([0-9a-f]+)\s+Length: 0x([0-9a-f]+)') { ,@([Convert]::ToInt64($Matches[1], 16), [Convert]::ToInt64($Matches[2], 16)) } })
                }
            }
            $entries.Add($e)
        }
    $vol = Get-Volume -Partition $part
    $info = fsutil fsinfo refsinfo $drive
    $manifest = [ordered]@{
        name = $Name; kind = 'refs'; scenario = $Scenario
        windows_build = [Environment]::OSVersion.Version.ToString()
        refs_version = (($info | Select-String 'REFS Volume Version') -replace '.*:\s*', '')
        cluster_size = $ClusterSize; sha256_checksums = [bool]$Sha256Checksums; integrity_streams = [bool]$IntegrityStreams
        label = $vol.FileSystemLabel; volume_size = $vol.Size; free = $vol.SizeRemaining
        serial = (($info | Select-String 'Volume Serial Number') -replace '.*:\s*', '')
        partition_offset = $part.Offset
        refsinfo = ($info -join "`n")
        entries = $entries
    }
    $manifest | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 (Join-Path $dir 'manifest.json')
    "$Name`: $($entries.Count) entries, ReFS $($manifest.refs_version), $ClusterSize-byte clusters"
} finally {
    Dismount-DiskImage -ImagePath $vhdx | Out-Null
}
