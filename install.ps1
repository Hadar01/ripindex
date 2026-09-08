#Requires -Version 5.1
<#
.SYNOPSIS
    ripindex installer for Windows.
.DESCRIPTION
    irm https://github.com/hadar01/ripindex/releases/latest/download/ripindex-installer.ps1 | iex

    Downloads the release archive for this machine, verifies it against the
    published SHA-256, and installs into %LOCALAPPDATA%\ripindex\bin (added to
    the user PATH). Any failure aborts without touching the install directory.
.PARAMETER Version
    Install a specific release, e.g. -Version 0.1.0. Defaults to the latest.
.PARAMETER To
    Install into this directory instead of the default.
.PARAMETER NoVerify
    Skip SHA-256 verification. Not recommended.
#>
[CmdletBinding()]
param(
    [string] $Version = $env:RIPINDEX_VERSION,
    [string] $To      = $env:RIPINDEX_INSTALL_DIR,
    [switch] $NoVerify
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Repo = 'hadar01/ripindex'
$Bin  = 'ripindex'

function Fail([string] $Message) { Write-Error $Message; exit 1 }

# TLS 1.2 for PowerShell 5.1 on older Windows, where it isn't the default.
try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch {}

$arch = $env:PROCESSOR_ARCHITECTURE
switch ($arch) {
    'AMD64' { $target = 'x86_64-pc-windows-msvc' }
    'ARM64' {
        # No native ARM64 build is published yet; the x86_64 build runs under
        # emulation on ARM Windows. Say so rather than silently installing it.
        Write-Warning 'No native ARM64 build yet - installing the x86_64 build, which runs under emulation.'
        $target = 'x86_64-pc-windows-msvc'
    }
    default { Fail "Unsupported architecture '$arch'. Build from source with 'cargo install $Bin'." }
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("ripindex-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

try {
    if ([string]::IsNullOrWhiteSpace($Version)) {
        $base = "https://github.com/$Repo/releases/latest/download"
    } else {
        $base = "https://github.com/$Repo/releases/download/v" + $Version.TrimStart('v')
    }

    # SHA256SUMS names every asset in the release, so it doubles as the way to
    # discover the versioned asset name when the version wasn't pinned.
    $sumsPath = Join-Path $tmp 'SHA256SUMS'
    try {
        Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sumsPath -UseBasicParsing
    } catch {
        Fail "Could not reach the release. Check https://github.com/$Repo/releases, or pass -Version X.Y.Z."
    }
    $sums = Get-Content $sumsPath

    if ([string]::IsNullOrWhiteSpace($Version)) {
        $line = $sums | Where-Object { $_ -match [regex]::Escape($target) -and $_ -match '\.zip$' } | Select-Object -First 1
        if (-not $line) { Fail "The latest release has no build for $target." }
        $asset = ($line -split '\s+')[-1].TrimStart('*')
    } else {
        $asset = "$Bin-" + $Version.TrimStart('v') + "-$target.zip"
    }

    Write-Host "installing $asset"
    $zip = Join-Path $tmp $asset
    try {
        Invoke-WebRequest -Uri "$base/$asset" -OutFile $zip -UseBasicParsing
    } catch {
        Fail "Download failed: $base/$asset"
    }

    if (-not $NoVerify) {
        $got  = (Get-FileHash -Path $zip -Algorithm SHA256).Hash.ToLowerInvariant()
        $line = $sums | Where-Object { $_ -match [regex]::Escape($asset) } | Select-Object -First 1
        if (-not $line) { Fail "No checksum published for $asset." }
        $want = ($line -split '\s+')[0].ToLowerInvariant()
        if ($got -ne $want) { Fail "Checksum mismatch for $asset (expected $want, got $got). Refusing to install." }
        Write-Host 'checksum ok'
    }

    $unpack = Join-Path $tmp 'unpack'
    Expand-Archive -Path $zip -DestinationPath $unpack -Force
    $exe = Get-ChildItem -Path $unpack -Recurse -Filter "$Bin.exe" | Select-Object -First 1
    if (-not $exe) { Fail "Archive did not contain $Bin.exe." }

    if ([string]::IsNullOrWhiteSpace($To)) {
        $To = Join-Path $env:LOCALAPPDATA "ripindex\bin"
    }
    New-Item -ItemType Directory -Path $To -Force | Out-Null
    Copy-Item -Path $exe.FullName -Destination (Join-Path $To "$Bin.exe") -Force

    Write-Host ''
    Write-Host "installed $Bin -> $(Join-Path $To "$Bin.exe")"

    # Prepend to the *user* PATH only - never the machine PATH, which needs
    # admin and affects other users.
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($null -eq $userPath) { $userPath = '' }
    $onPath = ($userPath -split ';' | Where-Object { $_ -eq $To }).Count -gt 0
    if (-not $onPath) {
        $newPath = if ([string]::IsNullOrEmpty($userPath)) { $To } else { "$To;$userPath" }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        $env:Path = "$To;$env:Path"
        Write-Host "added $To to your user PATH (restart your shell for it to apply everywhere)"
    }

    & (Join-Path $To "$Bin.exe") --version
    Write-Host ''
    Write-Host "Try:  $Bin search --root . TODO"
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
