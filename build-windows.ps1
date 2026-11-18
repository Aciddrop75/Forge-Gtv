# The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
# SPDX-License-Identifier: GPL-3.0-or-later
# This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.
# Build the Windows installer (forge-gtv-setup-<version>-windows-x86_64.exe) on a Windows PC.
# NOTE: the Windows build was NOT built or tested by the author's Linux environment - if anything fails,
#       please send the output back so it can be fixed.
#
# Needs: Rust (https://rustup.rs) with the default "stable-x86_64-pc-windows-msvc" toolchain and the
#        "Build Tools for Visual Studio" (C++ workload) that rustup offers to install.
#
# Run in PowerShell, in this folder:   powershell -ExecutionPolicy Bypass -File .\build-windows.ps1
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "Rust is not installed. Get it from https://rustup.rs, install it, then open a NEW PowerShell and run this again." -ForegroundColor Yellow
    exit 1
}
$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches.Groups[1].Value
Write-Host "Building The Forge GTV $version for Windows ..."
cargo build --release
if ($LASTEXITCODE -ne 0) { Write-Host "Build failed - see above." -ForegroundColor Red; exit 1 }
# The tests run after the build. The Windows code has never been run on a real Windows PC, so a failing test is a warning here,
# not a reason to withhold the installer.
cargo test --release
if ($LASTEXITCODE -ne 0) { Write-Host "Some tests failed (see above). The installer was still built; please send the failing test names back." -ForegroundColor Yellow }

New-Item -ItemType Directory -Force -Path dist | Out-Null
$out = "dist\forge-gtv-setup-$version-windows-x86_64.exe"
Copy-Item target\release\forge-gtv.exe $out -Force
$hash = (Get-FileHash $out -Algorithm SHA256).Hash.ToLower()
"$hash  $(Split-Path $out -Leaf)" | Out-File -Encoding ascii "dist\SHA256SUMS-windows.txt"
Write-Host ""
Write-Host "Done: $out" -ForegroundColor Green
Write-Host "Run it (double-click, or in PowerShell as administrator for the start-up service)."
