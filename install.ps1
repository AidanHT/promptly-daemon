#!/usr/bin/env pwsh
# Promptly installer (Windows).
#
# Downloads the latest prebuilt promptly + promptlyd from GitHub Releases, installs
# them, and adds the install directory to your user PATH. No Rust toolchain needed.
#
#   irm https://raw.githubusercontent.com/AidanHT/promptly-daemon/main/install.ps1 | iex
#
# Environment overrides:
#   PROMPTLY_VERSION       tag to install        (default: latest release)
#   PROMPTLY_INSTALL_DIR   install location      (default: %LOCALAPPDATA%\Promptly\bin)
$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$repo   = "AidanHT/promptly-daemon"
$target = "x86_64-pc-windows-msvc"
$installDir = if ($env:PROMPTLY_INSTALL_DIR) { $env:PROMPTLY_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Promptly\bin" }

$tag = $env:PROMPTLY_VERSION
if (-not $tag) {
  Write-Host "Resolving the latest release..." -ForegroundColor Cyan
  $tag = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
}
if (-not $tag) { throw "could not resolve the latest release (set PROMPTLY_VERSION=vX.Y.Z)" }

$asset = "promptly-$tag-$target.zip"
$url   = "https://github.com/$repo/releases/download/$tag/$asset"
$tmp   = Join-Path ([IO.Path]::GetTempPath()) ("promptly-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
  Write-Host "Downloading $asset ..." -ForegroundColor Cyan
  $zip = Join-Path $tmp $asset
  Invoke-WebRequest -Uri $url -OutFile $zip
  Expand-Archive -Path $zip -DestinationPath $tmp -Force

  $src = Join-Path $tmp "promptly-$tag-$target"
  New-Item -ItemType Directory -Force -Path $installDir | Out-Null
  Copy-Item (Join-Path $src "promptly.exe")  (Join-Path $installDir "promptly.exe")  -Force
  Copy-Item (Join-Path $src "promptlyd.exe") (Join-Path $installDir "promptlyd.exe") -Force
}
finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

Write-Host "Installed promptly + promptlyd $tag to $installDir" -ForegroundColor Green

$userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
if (($userPath -split ';') -notcontains $installDir) {
  [Environment]::SetEnvironmentVariable("PATH", "$installDir;$userPath", "User")
  $env:PATH = "$installDir;$env:PATH"
  Write-Host "Added $installDir to your user PATH (open a new terminal to pick it up)." -ForegroundColor Yellow
}

Write-Host "Done. Run 'promptly doctor' to verify your setup."
