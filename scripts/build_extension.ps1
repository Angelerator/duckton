#!/usr/bin/env pwsh
# Build the loadable Duckton DuckDB extension and append the metadata footer so it
# can be LOADed by the duckdb CLI on Windows. Mirrors scripts/build_extension.sh
# (the Python + duckdb calls are OS-agnostic; this just uses Windows paths / .dll).
#
# Usage:  scripts/build_extension.ps1 [-Release]
# Output: <repo>\dist\duckton.duckdb_extension
#
# Then:   duckdb -unsigned -c "LOAD '<repo>/dist/duckton.duckdb_extension'; `
#                              SELECT * FROM p2p_info();"
param([switch]$Release)
$ErrorActionPreference = 'Stop'

$profileFlag = ''
$profileDir = 'debug'
if ($Release) { $profileFlag = '--release'; $profileDir = 'release' }

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $repoRoot

Write-Host '==> building cdylib'
if ($profileFlag) { cargo build -p p2p-extension $profileFlag }
else { cargo build -p p2p-extension }
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$dylib = $null
foreach ($cand in @(
    "target/$profileDir/duckton.dll",
    "target/$profileDir/libduckton.dylib",
    "target/$profileDir/libduckton.so")) {
  if (Test-Path $cand) { $dylib = $cand; break }
}
if (-not $dylib) { Write-Error 'cdylib not found'; exit 1 }

$platform = (duckdb -list -noheader -c 'PRAGMA platform;').Trim()
Write-Host "==> platform: $platform"

New-Item -ItemType Directory -Force -Path dist | Out-Null
python scripts/append_extension_metadata.py `
  -l $dylib `
  -n duckton `
  -p $platform `
  -dv v1.0.0 `
  -ev 0.1.0 `
  -o dist/duckton.duckdb_extension
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host '==> wrote dist/duckton.duckdb_extension'
Write-Host '==> smoke test'
duckdb -unsigned -c "LOAD 'dist/duckton.duckdb_extension'; SELECT * FROM p2p_info();"
exit $LASTEXITCODE
