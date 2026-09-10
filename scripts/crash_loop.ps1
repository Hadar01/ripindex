# Kill-9 crash-consistency loop. Usage: scripts\crash_loop.ps1 [-Iterations 2000] [-Seed 1] [-DelayUs 400]
param(
    [int]$Iterations = 2000,
    [int]$Seed = 1,
    [int]$DelayUs = 400,
    [string]$Dir = (Join-Path $env:TEMP "ripindex-crash")
)
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
cargo build --release --manifest-path (Join-Path $repo "Cargo.toml") --bin crash-harness --features crash-harness
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& (Join-Path $repo "target\release\crash-harness.exe") run --dir $Dir --iterations $Iterations --seed $Seed --delay-us $DelayUs
exit $LASTEXITCODE
