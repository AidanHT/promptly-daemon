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

# The whole body runs in its own scope so `irm | iex` doesn't leak preference
# variables (or $repo/$tag/...) into the caller's interactive session.
& {
  $ErrorActionPreference = "Stop"
  $ProgressPreference = "SilentlyContinue"
  [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

  $repo   = "AidanHT/promptly-daemon"
  $target = "x86_64-pc-windows-msvc"
  $installDir = if ($env:PROMPTLY_INSTALL_DIR) { $env:PROMPTLY_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Promptly\bin" }

  $tag = $env:PROMPTLY_VERSION
  if (-not $tag) {
    Write-Host "Resolving the latest release..." -ForegroundColor Cyan
    try {
      $tag = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
    } catch {
      throw "could not resolve the latest release ($($_.Exception.Message)) - set PROMPTLY_VERSION=vX.Y.Z and re-run"
    }
  }
  if (-not $tag) { throw "could not resolve the latest release (set PROMPTLY_VERSION=vX.Y.Z)" }

  $asset = "promptly-$tag-$target.zip"
  $url   = "https://github.com/$repo/releases/download/$tag/$asset"
  $tmp   = Join-Path ([IO.Path]::GetTempPath()) ("promptly-" + [Guid]::NewGuid().ToString("N"))
  New-Item -ItemType Directory -Force -Path $tmp | Out-Null

  try {
    Write-Host "Downloading $asset ..." -ForegroundColor Cyan
    $zip = Join-Path $tmp $asset
    # -UseBasicParsing: ignored on pwsh, required on Windows PowerShell 5.1 boxes
    # where the Internet Explorer parsing engine isn't available.
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
    Expand-Archive -Path $zip -DestinationPath $tmp -Force

    $src = Join-Path $tmp "promptly-$tag-$target"
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null

    # A running daemon holds a lock on promptlyd.exe, so an upgrade-in-place
    # would fail halfway. Stop it first (promptly update does the same), and
    # rename any still-locked binary out of the way rather than dying on it.
    Get-Process promptlyd -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    foreach ($name in "promptly.exe", "promptlyd.exe") {
      $dest = Join-Path $installDir $name
      Remove-Item "$dest.old" -Force -ErrorAction SilentlyContinue
      if (Test-Path $dest) {
        try { Move-Item $dest "$dest.old" -Force } catch {}
        Remove-Item "$dest.old" -Force -ErrorAction SilentlyContinue
      }
      Copy-Item (Join-Path $src $name) $dest -Force
    }
  }
  finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
  }

  Write-Host "Installed promptly + promptlyd $tag to $installDir" -ForegroundColor Green

  # Read the user PATH unexpanded so entries like %USERPROFILE%\bin survive the
  # rewrite, keep the value kind, and broadcast the change so new terminals see it.
  $envKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $true)
  try {
    $userPath = [string]$envKey.GetValue("PATH", "", [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    if (($userPath -split ';') -notcontains $installDir) {
      $newPath = if ($userPath) { "$installDir;$userPath" } else { $installDir }
      $kind = if ($newPath -match '%') { [Microsoft.Win32.RegistryValueKind]::ExpandString } else { [Microsoft.Win32.RegistryValueKind]::String }
      $envKey.SetValue("PATH", $newPath, $kind)
      $env:PATH = "$installDir;$env:PATH"
      if (-not ("Promptly.Native" -as [type])) {
        Add-Type -Namespace Promptly -Name Native -MemberDefinition @'
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
'@
      }
      [UIntPtr]$result = [UIntPtr]::Zero
      # HWND_BROADCAST / WM_SETTINGCHANGE / SMTO_ABORTIFHUNG
      [Promptly.Native]::SendMessageTimeout([IntPtr]0xffff, 0x001A, [UIntPtr]::Zero, "Environment", 0x0002, 5000, [ref]$result) | Out-Null
      Write-Host "Added $installDir to your user PATH (open a new terminal to pick it up)." -ForegroundColor Yellow
    }
  }
  finally {
    $envKey.Close()
  }

  Write-Host "Done. Run 'promptly doctor' to verify your setup."
}
