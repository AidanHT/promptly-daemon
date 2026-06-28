#!/usr/bin/env pwsh
# Build (first run only) and start the Promptly telemetry daemon in the foreground.
#
#   ./run.ps1                      # watch the CURRENT directory (cd into a level first)
#   ./run.ps1 --workspace C:\path  # watch a specific level workspace
#   ./run.ps1 --api-port 8999      # any extra `promptlyd run` flags pass through
#
# Ctrl-C to stop. For a permanent background service use `promptlyd install`.
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$bin  = Join-Path $here "target\release\promptlyd.exe"

if (-not (Test-Path $bin)) {
    Write-Host "Building promptly + promptlyd (first run only)..." -ForegroundColor Cyan
    cargo build --release --manifest-path (Join-Path $here "Cargo.toml") -p promptlyd -p promptly
}

$forward = @($args)
if ($forward -notcontains "--workspace") {
    $forward = @("--workspace", (Get-Location).Path) + $forward
}

Write-Host "promptlyd  ->  API http://127.0.0.1:8765   OTLP http://127.0.0.1:4318   (Ctrl-C to stop)" -ForegroundColor Green
& $bin run @forward
