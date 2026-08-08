# Copyright 2026 OpenC3, Inc.
# All Rights Reserved.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
# See LICENSE.md for more details.
#
# This file may also be used under the terms of a commercial license
# if purchased from OpenC3, Inc.

# Build a NATIVE Windows installer (.msi via WiX) for the host architecture.
# Run this on Windows in PowerShell:
#
#   powershell -ExecutionPolicy Bypass -File .\package.ps1
#
# Output lands in dist\installers\.

$ErrorActionPreference = "Stop"
Set-Location -Path $PSScriptRoot

Write-Host "Packaging OpenC3 COSMOS for Windows $env:PROCESSOR_ARCHITECTURE"

# 1. Build the optimized release binary.
Write-Host "Building release binary..."
cargo build --release

# 2. Ensure the packaging tool is available.
cargo packager --version *> $null
if ($LASTEXITCODE -ne 0) {
    Write-Host "Installing cargo-packager..."
    cargo install cargo-packager --locked
}

# 3. Produce the WiX .msi installer.
$out = "dist\installers"
New-Item -ItemType Directory -Force -Path $out | Out-Null
Write-Host "Building installer into $out\ ..."
cargo packager --release -f wix --out-dir $out

Write-Host ""
Write-Host "Done. Installer in $out\:"
Get-ChildItem -Path $out -Include *.msi -Recurse | ForEach-Object { $_.FullName }
