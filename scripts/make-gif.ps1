#Requires -Version 5.1
<#
.SYNOPSIS
    Turn a screen recording into an optimised GIF for the README.
.DESCRIPTION
    Accepts whatever your recorder actually produced:

      * a recorder *project* archive (N-Studio / ScreenToGif .zip of PNG frames
        plus a JSON of per-frame delays) - frame timing is preserved,
      * a folder of numbered PNG frames,
      * a video file (mp4/mkv/webm/avi/mov).

    Needs ffmpeg on PATH. Uses palettegen/paletteuse, which is what makes a
    terminal GIF small: the palette is built from the actual frames rather
    than a fixed 256-colour web palette.
.PARAMETER Input
    The .zip, folder, or video to convert.
.PARAMETER Output
    Where to write the GIF. Defaults to docs/img/demo.gif.
.PARAMETER Fps
    Frames per second. 12-15 is plenty for a terminal; lower means smaller.
.PARAMETER MaxWidth
    Scale down to at most this width. 0 keeps the source width.
.PARAMETER Colors
    Palette size, 32-256. Terminal output needs far fewer than photos: 64
    usually looks identical to 256 at half the size.
.EXAMPLE
    scripts\make-gif.ps1 -Input docs\img\Project.zip
.EXAMPLE
    scripts\make-gif.ps1 -Input recording.mp4 -Fps 12 -MaxWidth 1000
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][Alias('In')][string] $InputPath,
    [string] $Output   = (Join-Path (Split-Path -Parent $PSScriptRoot) 'docs\img\demo.gif'),
    [int]    $Fps      = 14,
    [int]    $MaxWidth = 0,
    [ValidateRange(32, 256)][int] $Colors = 64
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not (Get-Command ffmpeg -ErrorAction SilentlyContinue)) {
    Write-Error "ffmpeg is not on PATH. Install it with:  winget install Gyan.FFmpeg"
    exit 1
}
if (-not (Test-Path $InputPath)) { Write-Error "No such input: $InputPath"; exit 1 }

$work = Join-Path ([IO.Path]::GetTempPath()) ("ripindex-gif-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work -Force | Out-Null

# ffmpeg writes progress to stderr; under 'Stop' that would abort the script
# even on success (PowerShell 5.1 wraps a native command's redirected stderr
# in ErrorRecords). Everything below is native invocation.
$ErrorActionPreference = 'Continue'

try {
    $item = Get-Item $InputPath
    $frameDir = $null
    $listFile = $null

    if ($item.PSIsContainer) {
        $frameDir = $item.FullName
    }
    elseif ($item.Extension -eq '.zip') {
        Write-Host "unpacking $($item.Name)..."
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $frameDir = Join-Path $work 'frames'
        [System.IO.Compression.ZipFile]::ExtractToDirectory($item.FullName, $frameDir)
    }

    if ($frameDir) {
        $pngs = Get-ChildItem $frameDir -Filter '*.png' | Sort-Object Name
        if ($pngs.Count -eq 0) { Write-Error "No PNG frames found in $frameDir"; exit 1 }
        Write-Host "$($pngs.Count) frames"

        # Per-frame delays, when the recorder recorded them. Without this a
        # variable-rate capture plays back at a uniform rate, which makes
        # typing look robotic and pauses vanish.
        $delays = @{}
        $meta = Get-ChildItem $frameDir -Filter '*.json' | Select-Object -First 1
        if ($meta) {
            try {
                $parsed = Get-Content $meta.FullName -Raw | ConvertFrom-Json
                if ($parsed.PSObject.Properties.Name -contains 'Frames') {
                    $i = 0
                    foreach ($f in $parsed.Frames) {
                        $name = Split-Path $f.Path -Leaf
                        $delays[$name] = [double]$f.Delay / 1000.0
                        $i++
                    }
                    Write-Host "read $i frame delays from $($meta.Name)"
                }
            } catch { Write-Warning "could not parse $($meta.Name); using a uniform $Fps fps" }
        }

        # concat demuxer: explicit duration per frame.
        $listFile = Join-Path $work 'frames.txt'
        # [char]92 is a backslash. Written this way on purpose: -replace takes a
        # regex, where a lone backslash is an invalid pattern, and the literal is
        # easy to mangle when this file is edited through other tooling.
        $sb = New-Object System.Text.StringBuilder
        [void]$sb.AppendLine('ffconcat version 1.0')
        foreach ($p in $pngs) {
            $d = if ($delays.ContainsKey($p.Name)) { $delays[$p.Name] } else { 1.0 / $Fps }
            if ($d -le 0) { $d = 1.0 / $Fps }
            [void]$sb.AppendLine("file '$($p.FullName.Replace([char]92, '/'))'")
            [void]$sb.AppendLine("duration $([Math]::Round($d, 3).ToString([Globalization.CultureInfo]::InvariantCulture))")
        }
        # The concat demuxer drops the final entry's duration, so repeat the
        # last frame or the closing pause is lost.
        [void]$sb.AppendLine("file '$($pngs[-1].FullName.Replace([char]92, '/'))'")
        [System.IO.File]::WriteAllText($listFile, $sb.ToString())
    }

    $scale = if ($MaxWidth -gt 0) { "scale='min($MaxWidth,iw)':-1:flags=lanczos," } else { '' }
    # ${Colors} must be braced: a colon straight after a variable name inside a
    # double-quoted string is read as a scope qualifier (as in $env:PATH), so
    # "$Colors:stats_mode" expands to nothing and ffmpeg sees "max_colors==diff".
    $filter = "fps=$Fps,${scale}split[a][b];[a]palettegen=max_colors=${Colors}:stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=3:diff_mode=rectangle"

    $outDir = Split-Path -Parent $Output
    if ($outDir -and -not (Test-Path $outDir)) { New-Item -ItemType Directory -Path $outDir -Force | Out-Null }

    Write-Host "encoding -> $Output"
    # Keep ffmpeg's own diagnostics: if it fails, its message is the only thing
    # that explains why, and swallowing it turns a clear error into a mystery.
    if ($listFile) {
        $ff = & ffmpeg -y -hide_banner -loglevel warning -f concat -safe 0 -i $listFile -vf $filter -loop 0 $Output 2>&1
    } else {
        $ff = & ffmpeg -y -hide_banner -loglevel warning -i $item.FullName -vf $filter -loop 0 $Output 2>&1
    }

    if (-not (Test-Path $Output)) {
        Write-Host '--- ffmpeg output ---'
        $ff | ForEach-Object { Write-Host "  $_" }
        Write-Error 'ffmpeg produced no output (see above).'
        exit 1
    }

    $sizeMB = (Get-Item $Output).Length / 1MB
    Write-Host ''
    Write-Host ("wrote {0}  ({1:N2} MB)" -f $Output, $sizeMB)
    if ($sizeMB -gt 2) {
        Write-Warning "Over 2 MB - the README will feel slow. Try:"
        Write-Warning "  scripts\make-gif.ps1 -InputPath $InputPath -Fps 10 -MaxWidth 900 -Colors 48"
    }
}
finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
