# Build script for FreeClip 2 空间音频
# Usage:
#   .\build.ps1              # standalone exe (src-tauri\target\release\freeclip2-spatial.exe)
#   .\build.ps1 -Installer   # exe + MSI installer

param(
    [switch]$Installer
)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

Write-Host "==> Checking toolchain" -ForegroundColor Cyan
foreach ($cmd in @("node", "npm", "cargo")) {
    if (-not (Get-Command $cmd -ErrorAction SilentlyContinue)) {
        throw "Missing required tool: $cmd (install Node.js and the Rust toolchain first)"
    }
}

if (-not (Test-Path "node_modules")) {
    Write-Host "==> Installing npm dependencies" -ForegroundColor Cyan
    npm install
}

# tauri-build caches the embedded exe icon; touching build.rs forces a re-embed.
Write-Host "==> Refreshing Windows icon resource" -ForegroundColor Cyan
(Get-Item "src-tauri\build.rs").LastWriteTime = Get-Date

if ($Installer) {
    Write-Host "==> Building release + MSI installer" -ForegroundColor Cyan
    npx tauri build
} else {
    Write-Host "==> Building standalone exe" -ForegroundColor Cyan
    npx tauri build --no-bundle
}

Write-Host ""
Write-Host "Done:" -ForegroundColor Green
Get-ChildItem "src-tauri\target\release\freeclip2-spatial.exe" | ForEach-Object {
    Write-Host ("  " + $_.FullName + "  (" + [math]::Round($_.Length / 1MB, 1) + " MB)")
}
if ($Installer) {
    Get-ChildItem "src-tauri\target\release\bundle\msi\*.msi" -ErrorAction SilentlyContinue | ForEach-Object {
        Write-Host ("  " + $_.FullName)
    }
}
