#Requires -Version 5.1
<#
.SYNOPSIS
    Drives the README demo at a watchable pace for a screen recorder.
.DESCRIPTION
    The PowerShell counterpart to record-demo.sh, for recording on Windows
    without Git Bash. Start your recorder (ScreenToGif, N-Studio, LICEcap),
    run this, stop the recorder.

    Unlike the shell version this uses Start-Sleep, a cmdlet rather than an
    external process, so per-character typing costs nothing extra and the
    pacing is smoother.

    Recommended: a ~110x28 window, font size 16-18, recording the terminal
    region only. Default pacing gives roughly a 25 second recording.
.PARAMETER Corpus
    Directory to search. Defaults to $HOME\cpython (see scripts/demo-corpus.sh).
.PARAMETER Speed
    Pacing multiplier. 1.0 is the default; 1.4 is slower and more deliberate,
    0.7 is brisk.
.EXAMPLE
    scripts\record-demo.ps1
.EXAMPLE
    scripts\record-demo.ps1 -Corpus C:\code\bigrepo -Speed 1.3
#>
[CmdletBinding()]
param(
    [string] $Corpus = (Join-Path $HOME 'cpython'),
    [double] $Speed  = 1.0
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $repoRoot 'target\release\ripindex.exe'

if (-not (Test-Path $bin)) {
    Write-Error "No release binary at $bin. Run: cargo build --release"
    exit 1
}
if (-not (Test-Path $Corpus)) {
    Write-Error @"
No corpus at $Corpus

Clone one somewhere short (result paths are absolute, so a long path wraps
badly in the recording):

    git clone --depth 1 -b v3.12.0 https://github.com/python/cpython `$HOME/cpython
"@
    exit 1
}
$Corpus = (Resolve-Path $Corpus).Path

# Result paths are absolute, so a long corpus path dominates the frame.
if ($Corpus.Length -gt 40) {
    Write-Warning "Corpus path is $($Corpus.Length) characters: $Corpus"
    Write-Warning "Absolute result paths will wrap badly in the GIF. Consider a shorter path."
    $reply = Read-Host 'Continue anyway? [y/N]'
    if ($reply -notmatch '^[yY]') { exit 1 }
}

# Show `ripindex ...` in the typed commands rather than an absolute build path.
$env:Path = (Split-Path -Parent $bin) + ';' + $env:Path

function Nap([double] $Seconds) {
    Start-Sleep -Milliseconds ([int]($Seconds * $Speed * 1000))
}

# Start-Sleep is a cmdlet, not an external process, so a per-character delay
# is affordable here - the shell version has to batch characters to avoid
# paying ~70ms of process spawn per sleep.
function Type-Command([string] $Text) {
    Write-Host '$ ' -NoNewline -ForegroundColor Green
    foreach ($ch in $Text.ToCharArray()) {
        Write-Host $ch -NoNewline
        Start-Sleep -Milliseconds ([int](28 * $Speed))
    }
    Write-Host ''
}

function Say([string] $Text) {
    Write-Host $Text -ForegroundColor DarkGray
}

function Run([string] $Command, [double] $Dwell = 1.8) {
    Type-Command $Command
    Nap 0.3
    try { Invoke-Expression $Command } catch { }
    Nap $Dwell
}

# Index off-camera: the recording shows the warm path, which is what daily use
# feels like. The first-run cost is stated honestly in the README's benchmark
# table rather than hidden here.
# PowerShell 5.1 wraps a *native* command's stderr lines in ErrorRecords when
# the stream is redirected, and under $ErrorActionPreference = 'Stop' that
# aborts the script even when the exe exited 0. Setup validation above is done,
# so drop to Continue for the rest, which is all native invocations.
$ErrorActionPreference = 'Continue'

Write-Host 'preparing (indexing off-camera, a few seconds)...' -NoNewline
& $bin index $Corpus 2>&1 | Out-Null
Write-Host "`r$(' ' * 55)`r" -NoNewline

# Run from inside the corpus so the typed commands read `--root .` rather
# than carrying an absolute path across the frame. Output is unchanged:
# result paths print relative to the root either way.
Set-Location $Corpus

Clear-Host

# Count via git when the corpus is a repo: instant, and it excludes .git for
# free. Get-ChildItem -Recurse -Force here was pathological - it materialises
# an object per file including every .git object, which took minutes and over
# a gigabyte of RAM on this corpus.
$fileCount = $null
try {
    $tracked = & git -C $Corpus ls-files 2>$null
    if ($LASTEXITCODE -eq 0 -and $tracked) { $fileCount = @($tracked).Count }
} catch { }
if ($null -eq $fileCount) {
    # Not a git repo: enumerate lazily and prune .git, rather than buffering.
    $fileCount = 0
    foreach ($f in [System.IO.Directory]::EnumerateFiles($Corpus, '*', 'AllDirectories')) {
        # Built from DirectorySeparatorChar rather than a regex: a literal
        # backslash pattern is easy to get wrong and needs no escaping here.
        $sep = [System.IO.Path]::DirectorySeparatorChar
        if (-not $f.Contains("$sep.git$sep")) { $fileCount++ }
    }
}
Say "# CPython source: $fileCount files, ~90 MiB. Indexed once, now warm."
Nap 1.1

Run "ripindex search --root . PyUnicode_FromString -n 3"

Clear-Host
Say '# Boolean queries, phrases and negation - not just literals:'
Nap 0.7
Run "ripindex search --root . 'asyncio AND subprocess' -n 3"

Clear-Host
Run "ripindex search --root . '`"reference count`"' -n 3"

Clear-Host
Say '# A daemon keeps it current and answers every query:'
Nap 0.7
Run 'ripindex status' 2.5
